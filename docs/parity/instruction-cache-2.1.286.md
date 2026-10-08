# Instruction context cache in Claude Code 2.1.286

The current production port owns one instruction cache per explicit root
orchestrator. A typed current `SessionId` selects the Gv context entry; child
budget/transcript identifiers and cwd do not create another Gv authority. The
eager qb file cache is independent. This is the latest contract only: the raw
`instruction_context_provider(memory)` factory has been removed, without an
older-version alias or fallback.

Production sources are frozen. The saved native oracle passed `--check` for
**20 scenarios and 300 deterministic operations**. All **5 Core cache tests
passed within the 743-test Core full run**; all **16 Orchestrator cache tests
passed within the current 1,277/1,277 full run**. The current Agent full run
passed **595/595**, including initial Full-load failure and current SDK contract
regressions. These results establish the scoped production regressions, rather
than complete native-session or live host acceptance.

## Native evidence and reproducible check

The installed darwin-arm64 reference has SHA-256
`75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
The generator executes extracted native helpers from that binary. Its seven
absolute, end-exclusive byte ranges and their hashes are recorded in
[`instruction_cache_2_1_286.json`](../../crates/orchestrator/tests/fixtures/instruction_cache_2_1_286.json).
The selected ranges cover At, R7/qb, eager invalidation and hook cause, DN/Gv
and projections, and Ctn/pT/subscription/reason helpers. The generator checks
both the binary hash and range hashes; it does not substitute hand-written
expected cache transitions.

```sh
node scripts/tests/instruction_cache_286_oracle.mjs /path/to/2.1.286 --check
```

[`instruction_cache_286_oracle.mjs`](../../scripts/tests/instruction_cache_286_oracle.mjs)
substitutes event-controlled asynchronous aLn/yLn loaders, deterministic
feature-state subscriptions, and recorded classifier/project/telemetry effects.
Completion and rejection are explicit events with Promise microtasks; there
are no wall-clock assertions. Native aLn traversal, yLn account/project/plugin
acquisition, instruction rendering, live provider calls, and real host callback
registration are outside this oracle's execution scope. The fixture states
those substitutions explicitly.

A separate read-only Node VM probe executed the actual LMe and Hot catches for
six errno values each (ENOENT, EACCES, ENOTDIR, EISDIR, EIO, EMFILE), using
controlled readdir/read rejection, canonical resolution, native health state
and event sinks. All **12 probes resolved with soft empty results**. This was an
ad hoc investigation, not a checked-in oracle or additional persisted suite
coverage. LMe's outer catch consumes unexpected readdir errors and records
`rules_walk_failed`; Hot records `read_eacces`/permission for EACCES and
`read_failed` for EIO/EMFILE without rejecting the file load. Native health gates
belong to the host, rather than producing an independent event for every file.
The probe used native source spans LMe `[665866, 668019)`, Hot
`[662907, 663534)`, XDn `[661952, 661982)` and Uot `[661982, 662368)` in
`src_183727495.js`. It did not execute full acquisition or provider I/O.

The separately persisted
[`memory_utf8_286_oracle.mjs`](../../scripts/tests/memory_utf8_286_oracle.mjs)
passed `--check` for **27 cases: 20 byte inputs, 3 stat guards and 4 backend
results**. Its hash-pinned
[`memory_utf8_2_1_286.json`](../../crates/memory/tests/fixtures/memory_utf8_2_1_286.json)
records actual native l0/QDn/Hot execution. Node fs and Buffer decoding stand in
for Bun builtins; backend results and Hot's downstream markdown parser are
controlled seams. It covers lossy UTF-8 replacement, BOM/NUL, byte-view offsets,
4 MiB guards and backend absence/error results. Native `$ot` does not expose a
`size_bytes` field; Rust's local metadata counts the original raw bytes read.
The memory library passed **196/196**. These are native-source helper and Rust
library checks, not native CLI/Bun acceptance. They do not turn the earlier
12 ad hoc soft-skip probes into persisted coverage or validate host health
reporting and the remaining acquisition callbacks.

Additional source evidence, using extracted chunk character offsets:

| Contract | Native source |
| --- | --- |
| Root-keyed At and withProject identity | `src_175995718.js`, 49355–52652 |
| qb passes the root to aLn and keeps the external-mode slot separate | `src_183727495.js`, 670209–670326 |
| Gv context, descriptor and managed projections | `src_183727495.js`, 681900–686786 |
| Main initial context acquisition | `src_201906808.js`, tIn 1068–1595; Uur awaits it at 2052 |
| Child initial Gv acquisition and omit projection | `src_191018893.js`, Lv 121956–124550 and Fl 145417–145773 |
| Or descriptor/routing fallback | `src_191662485.js`, 17135–18609 |
| Compaction pT followed by Vot/KPt | `src_183727495.js`, around 1906090–1906500; Vot/KPt at 676250–676449 |
| Conditional C7 performs a fresh LMe walk | `src_183727495.js`, 678797–679581 |

## Implemented production behavior

`core::host::instruction_context_cache` retains one detached asynchronous build
and all its projections. Cancelling a first waiter does not cancel the build.
Success and rejection both stay cached until the corresponding explicit event;
failure does not trigger a filesystem retry. A build reserves its original
file-cache handle before detached polling, so a refresh cannot substitute a
new file snapshot for that already admitted build.

The cache supports these distinct native events:

| Event | Context and file behavior |
| --- | --- |
| pT | Clear every cached context entry under this root, retain qb files, and set a reason only for an entry which was loaded or pending. A second event while unloaded retains the first reason. |
| WC | Clear qb file slots, retaining already admitted Gv context and file projections. |
| WC plus pT | One root lock clears both for a compound refresh event. |
| Ctn | Clear context identities and immediately fence all already-started classifier completions; retain qb and the once-per-root plugin telemetry gate. |
| KPt | Rearm the eager hook cause, advance walk epoch, and clear qb; it does not itself invalidate Gv. |
| Compaction | `apply_compaction()` executes pT(`compaction`) then KPt(`compact`) under one root lock. |
| Fresh session | `reset_session_context()` executes Ctn then KPt(`session_start`) under one root lock. |

pT does not immediately fence an older classifier completion. That completion
can publish until a newer completed build advances the accepted ordinal. Ctn
fences it immediately while its original waiter can still receive the result.
The Core fixture and lifecycle regression exercise this distinction. The
production root does not currently install a native classifier/project/plugin
acquisition observer; those semantic state tests are not live publication
acceptance.

Main prompt consumers use the same Gv files and context. Query preparation
passes one original load handle to the result-bearing scalar projection before
compaction. Inline prefix, ordinary retry and reattachment retain the prepared
message identity and bytes. Lazy sent paths and attachment history remain a
separate cursor. A late old build must pass typed session, current cache and
origin checks before changing that cursor. Async clear/resume reset preserves
old Gv/qb authority until the actual activation commit holds both session and
cursor guards. That synchronous commit publishes identity beside Ctn/KPt,
including same-ID resume. Both Gv and independent qb reserve their original
handles while holding the current identity guard, then release it before any
loader await; child ingress does not acquire the turn gate. Explicit InstructionsLoaded hooks
read the current independent qb view and consume its real eager cause.

`RootInstructionContextProvider` is bound once through a weak reference to the
actual root. Both child Full and ManagedOnly read its current Gv, including
after root session retargeting. ManagedOnly filters the same descriptors and
replaces only the instruction body; it retains the other context fields and
date. Descriptor absence preserves the full context, and a successful empty
managed body removes `instructions`. Child caller cwd remains relevant to fresh
after-Read discovery, not to eager cache identity. A pending child reader drops
its strong root reference before awaiting the detached build. Explicit fork
user-context overrides retain their native bypass.

The Runtime identity adapter reads the real first-party Anthropic account,
preserves its raw email, and omits identity on logout instead of falling back to
stale configured data. Its weak account observer synchronously sends
pT(`account_change`) to the same owning root, without WC. Other LLM provider
identities are not coerced into this field. The retained eager file Promise is
therefore independent of account refresh.

Initial failure and routing fallback are separate contracts. Native tIn and
Lv await Gv before Or/Fl routing. An initial rejected Gv aborts acquisition;
Or's later descriptor/routing catch can choose inline or bare-announced output,
and an unavailable managed projection can retain the previously acquired full
context. Main model preparation now propagates initial acquisition failure
before dispatch. The Agent authoritative Full-load Result migration and its
runner failure coverage passed the current 595/595 Agent full run; an initial
failure must not silently reuse an inherited context. The
[`current-only-sdk-contracts.md`](current-only-sdk-contracts.md) records the
single required subagent stream, required fresh conditional acquisition and
weak auth observers, and explicit missing-backend failures instead of synthetic
successful completion.

## Verification boundaries and remaining gaps

The 20 native scenarios cover single-flight and descriptor identity, ordinary
edits, sticky context/file rejection, all-session root isolation, first-reason
retention, separate pT/WC/both views, clear and both classifier completion
orders, hook cause/epoch, memory subscription/unsubscription, account purge,
plugin once-only telemetry and external-mode file slots. The Core replay uses
all 300 operations. Its additional tests exercise first-waiter cancellation,
file-scope isolation, invalidation before first producer polling, and compound
lifecycle pending handles. The 13 Orchestrator cases cover actual main
consumers, refresh reasons, clear and retired-build authority, account bytes,
query prefix/reattachment, child projections/root lifetime, and rejection before
model dispatch. Three further actual RootProvider races cover clear activation,
same-ID resume and ID-read-through-reservation with retained original waiters,
bringing the Orchestrator total to 16. All 16 passed within the current
1,277/1,277 full run, including the corrected additional-context fixtures.

Native conditional C7's confirmed stale unsent-rule body/deletion/glob issue
has a production repair: `MemoryHierarchyProvider::load_conditional_rules` is a
required capability, and the old main rule cache is removed. Each trigger reads
fresh Managed/User/cwd-level conditional rules with live cwd and the existing
shared sent-path dedup; it does not reread full top-level instruction files or
qb. New rules could often be recovered by the old later nested discovery, so
the defect was not universal invisibility of newly added rules. Four actual
filesystem/collect_turn_reminders regressions and two helper scope/freshness
regressions passed within the full Orchestrator run. The eager Gv cache and its
session activation boundaries remain independent of that fresh rule lane.

The fresh conditional C7 cases above passed within Orchestrator's current full
run: four actual filesystem/reminder cases and two helper scope/freshness cases.
Related full runs passed **sidequery 60/60** and **compaction 275/275**. Current
integration filters passed **force_compact_real 17**, **memory_seed 2**, and
**nested_discovery 8**. The strict `compact_persistence` rerun passed **4/4**:
context/date/token-budget exact payload bytes, physical order, hot UUIDs,
preserved tail and the complete cold-resume parent chain remain asserted.
Runtime pool starvation passed **3/3** with actual model-entry, pending-future
and cancellation-drop proof. Current Runtime/LLM Runtime/Orchestrator/Tasks
full libraries passed **1,386 (one ignored)/1,030/1,277/534**, including the real
desktop writer-release/resume regressions after required Weak reporting
admission broke the ownership cycle. These are scoped regression results;
native 2.1.287 deltas are tracked in the [current register](claude-code-2.1.287.md).

- PolicyRefresh requires the real policy-helper refresh emitter and the native
  projected-value comparison: policy/helper `claudeMd`, excludes, auto-memory,
  env, and ordered managed `[path, content]` rows. A generic settings JSON change
  is not that event; errors must not advance its baseline. Host wiring remains
  incomplete.
- SettingsSync's native home-seed Apply callback is absent. Disk writes or a
  generic settings watcher do not establish that callback.
- HooksInvalidate requires the trusted plugin control-context
  `refreshContext` callback. Plugin reload and hook additionalContext are not
  substitutes; that host capability is absent.
- Memory-pause/resume and auto-memory off/on live controls are absent. Typed
  atomic refresh support and simulated native subscriptions do not wire them.
- Ordinary native file/rule read errors are soft skips. LMe's outer catch,
  Hot's read handler and JY's import path do not establish a requirement to
  change the Full provider's Vec result into a Result. The host-owned native
  health/permission reporting and other acquisition boundaries remain
  unaccepted: aLn's UDn read callback, canonical-root warmup, prefetch completion
  and recordContextBuild/internal producer exceptions; yLn's plugin context
  resolver and dispatcher fallback. Those producers are not executed by the
  cache oracle. Attached-project request/timeout failures are caught and return
  null in native IZo, so they are not assumed fatal. Explicit Result-bearing
  custom/managed provider failures still retain their initial sticky rejection
  and must propagate before model dispatch.
- Attached-project and Perforce acquisition, native policy-verdict/classifier
  producers and plugin instruction acquisition remain absent. No synthetic
  producer or policy update was added to make semantic-state tests look live.

This scope does not establish a complete native session, real provider,
physical-device or packaged-app acceptance. The overall latest-version parity
goal remains open.
