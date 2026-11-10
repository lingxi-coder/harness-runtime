# Local memory and dream alignment — Claude Code 2.1.319

The official [latest release](https://github.com/anthropics/claude-code/releases/tag/v2.1.319)
was checked on 2026-11-09. All extraction and oracle evaluation use one pinned
darwin-arm64 executable, not a moving installation:

- Size: `321995200` bytes.
- SHA-256: `1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
- Native version capture: `ev_d1515eaf7d76503ceb4a8a85c097629b0f29ea9e9af196f024ce94627fa72e6c`.
- Focused extracted-JavaScript analysis: `ev_8e996a6abc699a951c968cdf0ca7bf41dd35160541d8767db4a7b9f694063966`.

Implementation base: `968fd178`, isolated branch
`codex/memory-dream-319-20261109`. The concurrent product checkout is unchanged.

Each checked-in oracle records original byte offsets and source hashes, rejects
a different executable, evaluates the original selected functions in a VM with
declared external host facts, and compares exact output. Whitespace, UTF-16
units, ordering, and source-schema property order are not normalized.

## Evidence and implementation

| Surface | Executable oracle | Production boundary |
| --- | --- | --- |
| Lean, rewritten hygiene, standard and static memory protocols; update source labels | `memory_prompts_319_oracle.mjs` | `prompt/memory_section.rs`, `prompt/memory_update.rs` |
| Memory gate priority, index truncation/warnings, frontmatter, custom paths, recalled-file caps, Unicode/tag sanitation | `memory_storage_319_oracle.mjs` | `memory/{gate,index_cap,prefetch}.rs`, shared instruction sanitizer |
| File-extraction directive, cursor/prose/write gates, inventory, denial text | `session_memory_319_oracle.mjs` | `memory/auto_memory_extract.rs`, `conversation/auto_memory.rs` |
| Consolidation directive and all renderer branches | `dream_319_oracle.mjs` | `tasks/handlers/dream_prompt.rs` and checked-in templates |
| Availability/settings gates, cadence, session catalog, PID lock, rollback and scheduler context | `auto_dream_319_oracle.mjs` | `tasks/auto_dream.rs` |

All scripts are under `scripts/tests/` and accept
`<absolute-path-to-pinned-executable> --check`. Fixtures contain 270 storage
vectors and 2,371 generated Unicode readings; four memory templates, nine
directory renderings and eight updates; 18 dream renderings; extraction prompt,
trigger, inventory and denial vectors; and independent scheduler scenarios.

## Runtime behavior

Desktop and Mobile compose the same project memory directory for instructions,
recall, extraction and consolidation. They honor the active brand's configured
home, native custom-directory validation, trusted source priority, and canonical
project identity. Mobile retains its stable logical project namespace across
host mount changes. A dormant store creates and reads no memory files. Enabling
memory and changing its directory reuse the session, cancel old work, refresh
the eager index and leave previous stores intact. Settings are sampled again
after a foreground turn before scheduling its trailing writers. An unavailable
settings source suppresses admission independently of environment overrides;
hot disable also fences pending legacy session-note commits.

The eager `MEMORY.md` read follows native frontmatter/comment removal,
truncation, then sanitation. Index byteCount is the native UTF-16 length;
truncation keeps the original warning text and source units. The index appears
as an AutoMem instruction descriptor and is excluded from topic-file recall.

Post-turn file extraction is a five-turn, tool-executing fork. It uses the native
own-user prose, cursor, direct-main-write and cadence rules, coalesces a trailing
snapshot, and reports actual successful writes. Automatic consolidation uses
the native 24-hour/five-session defaults, ten-minute scan throttle, UUID-only
session catalog, one-hour live-PID guard and previous-timestamp rollback.
Cancellation, failed admission and session changes fence stale work. Embedded
conversations share a PID, so local lock mutations are serialized within the
process while retaining the native PID file bytes.

Both workers inherit the parent's paired model/profile, source system vector,
ordered tool schemas, typed attachment identities and task-delivery metadata.
The child receives its directive once. Both internal forks suppress transcript
writing and ordinary Agent lifecycle presentation. A native automatic Dream row
is marked notified before activation: only its memory-update attachment enters
model context. Its registry row is retained until the scheduler consumes the
terminal result, preventing a notification sweep from turning success into a
rollback. Manual legacy Dream notifications retain their previous behavior.

The headless invoker validates exact tools, literal shell commands and memory
paths, then grants only the validated call. Configured deny/ask rules and hook
confirmation requirements remain authoritative. A rule preflight runs before
extraction cost or consolidation lock acquisition. Successful validated removals
count as dream changes; extraction retains its native Write/Edit-only accounting.

Review fixes extend file-tool containment only for that exact admitted call:
the memory root reaches Read/Write/Edit/Glob/Grep without modifying session
trusted directories. Dream transcript roots are read-only. Trusted schema
normalization is rebound immediately around the tool body; rewritten, unrelated,
or revoked invocations cannot inherit those roots. The real-tool regression
uses separate workspace, memory and transcript directories, and retargeting
cannot keep reading the old memory store.

Leaf symlinks and non-Markdown canonical targets are rejected. Literal Bash
removal follows Bash double-quote escaping and rejects parent traversal;
PowerShell admits a conservative literal reader/parameter subset. A finishing
dream retains its scheduler ownership guard through publication or rollback,
so shutdown joins it before a reset drains old updates.

The second review fixes pair private file-read state with the exact frozen
conversation history, before extraction inventory or dream admission can await.
Each execution receives an independent mutable copy of that immutable seed;
workers with no paired history start with an empty cache. Private reads never
become foreground reads or another worker’s deduplication baseline. Successful
Write/Edit calls retain the foreground’s old bytes and mark changed views stale,
including alternate path spellings, so unseen background content cannot authorize
a stale foreground overwrite.

Dream completion retains a finishing record through spool and terminal
publication. A stop that loses to completion waits for the actual terminal row;
shutdown separately joins finishing hooks. Scheduler shutdown runs under a
retained owner and receipt, so dropping or timing out a waiter does not discard
lease rollback or registry completion-hold release. A retry joins that same
settlement. An unexpectedly failed owner propagates failure rather than claiming
completion.

For standard local memory with dynamic system sections excluded, production
assembly now uses the path-free static protocol and places the exact directory
announcement last in dynamic user context. Inline, Announced and inherited fork
contexts retain it. Announced context emits a new directory after live enable or
retarget while continuing to suppress duplicate stable sections.

The third review fixes retire a recorded memory protocol on enable, disable or
retarget, and replace its source vector while retaining tool descriptions and
context-rendering metadata. Cold resume checks the restored protocol against
current preferences. Missing resumed snapshots remain missing. Background
workers wait until a real main request publishes the matching memory generation;
prompt previews, old replies and compaction saves cannot reopen admission. This
also covers path-free static protocols whose directory lives in user context.

Both workers bind the shell name and dialect to the host's advertised tool.
Mobile uses `Shell` for literal readers and validated removal. Removal receives
an exact-call capability for the host-owned memory root and executes through
pinned, no-follow filesystem operations after the ordinary Registry permission
gate. It does not depend on mounting the private host memory store in the guest.
Revocation is checked before each removal; deny and ask rules remain effective.

The fourth review fix binds mutation authority to the worker's original root,
the admitted parent directory and the existing leaf identity. Permission waits
cannot admit a replacement root, ordinary directory exchange, changed leaf or
revoked scope. Missing parents and leaves are created exclusively. Existing
files remain single-link regular files before direct truncation. Write/Edit
retain their normal read-state, encoding and history bookkeeping.

Literal removal is validated once and executed through the same rooted host
operation for Bash, PowerShell and Mobile Shell after Registry checks. A failed
admission never becomes a raw shell call. Cancellation and forced-missing-file
semantics are preserved, as are each shell's native result fields.

Native rollout defaults remain intact: `tengu_passport_quail` defaults false;
`tengu_onyx_plover` defaults null, and an explicit dream setting cannot create
server availability. These are separate from ordinary `autoMemoryEnabled`.

## Deliberate product differences and limits

- Branding substitutions are limited to `CLAUDE.md` → `LINGXI.md`, `Claude Code`
  → `LingXi`, and the named session-brand phrase in the dream prompt. Real
  configured paths replace host path values; the product keeps `memdir`.
- Native `.319` source admits trusted project/local `autoMemoryDirectory`
  layers, despite an older public field description saying project settings are
  always ignored. Untrusted workspace layers remain excluded.
- Vendor Cowork path overrides, account-backed team/MCP memory, and remote
  memory-service CAS are not advertised by this local-file implementation.
- LingXi's existing four-tier retrieval/retention and legacy session-document
  extractor remain product extensions. They are not relabeled as the new native
  file extractor, and compaction retains its existing session-note integration.
- REA's static graph has unresolved external/dynamic relations; it establishes
  extracted artifact identity and syntax, not execution of every branch.
  VM differential tests and Rust production tests provide separate evidence.
- Transport captures compare controlled original source vectors and metadata.
  CacheSafeParams does not retain every possible request-local cache/suffix
  override, so universal provider-wire equivalence is not claimed.
- No paid-model call, packaged Desktop acceptance, physical-device acceptance,
  or long-running soak result is implied by these checks.

## Validation

The fourth-review mutation fix was verified on macOS using an isolated build
with all 87 local-package fingerprints invalidated. Formatting and
`git diff --check` pass. The integrated command was:

```sh
cargo test --locked --offline -p rooted-fs -p core -p memory -p tool-api -p tool-file -p tool-shell-mobile -p orchestrator --lib memory -- --skip resume::nested_memory_resume_287_tests::persisted_nested_memory_has_identical_hot_and_cold_payload_and_projection
```

| Fourth-review verification | Result |
| --- | --- |
| Integrated memory filter across the seven listed packages | 177 passed |
| Complete memory library | 233 passed |
| Complete rooted-filesystem library | 40 passed |
| Complete tool API library | 312 passed |
| Complete file-tool library with this command's default features | 398 passed |
| Complete Mobile Shell library | 27 passed |

The complete library checks used the fresh test binaries emitted by the
integrated command, without a test-name filter. The new regressions block
permission evaluation before changing parents, roots, leaves or worker state.
They cover symlink and ordinary directory replacement, protected subtrees,
new and missing-parent writes, hard links, later calls on substituted roots,
revocation and cancelled pruning. The controls retain Read-to-Write/Edit,
quoted multi-file removal, repeated forced removals and missing-file behavior.
The original filesystem transition no longer reaches a changed destination.
Fresh candidate review identified additional directory, lifetime, fallback and
cancellation variants; those variants were corrected before the final run.
Windows runtime and physical-device behavior were not exercised in this pass.

The five native-oracle `--check` commands and the wider Desktop/Mobile feature
run below are earlier verification of the unchanged prompts and runtime wiring:

| Check | Result |
| --- | --- |
| Complete memory library on macOS, including shell/path regressions | 233 passed |
| Dream handler and scheduler, including completion/stop and cancelled-waiter barriers | 29 passed |
| Complete file-tool library, including ordinary trusted-root behavior | 433 passed |
| File-read-state cache, including private memory forks and foreground invalidation | 26 passed |
| Instruction announcements, including live memory-directory changes | 9 passed |
| Complete Mobile Shell library | 27 passed |
| Integrated memory filter across core, agent, memory, tools, orchestrator, tasks and Desktop/Mobile | 197 passed |
| Native storage corpus integration (earlier verification; unchanged fixtures) | 7 passed |
| Admission-time registry completion/notification integration (earlier verification) | 2 passed |

Logs are under `artifacts/memory-dream-319-20261109/work/` in the enclosing
workspace: `review3-fixes-memory.log`, `review3-fixes-dream.log`,
`review3-fixes-file.log`, `review3-fixes-read-state.log`,
`review3-fixes-announcements.log`, `review3-fixes-mobile-shell.log`, and
`review3-fixes-integrated-r1.log`.
The five freshly repeated native checks are in `review3-fixes-oracles.log`.
Earlier storage/registry integration is in `fixture-integration-final.log`.
The integrated run includes 12 runtime tests with both `desktop,mobile` features.
The non-UTF-8 filesystem fixture is Linux-only; macOS rejects that fixture before
the boundary can inspect it. `git diff --check` and the exported patch's
application check against the unchanged base index pass.

Independent re-review approved the original five fixes, the four second-review
fixes and both third-review fixes. The prompt regression covers live and cold
resume, missing snapshots, both dynamic-section modes, stale request publication,
and prepared-but-dropped requests followed by compaction. The Mobile regression
uses the real registered Shell with allow, deny and ask policies and separate
host-memory and workspace roots. The original shell/path attack fixtures are rejected by the patched helpers;
ordinary in-root removal remains admissible. Project artifacts copied from a
concurrent checkout were invalidated before the final integrated build so its
results use this worktree's sources.

The broad orchestrator memory filter also reaches an unchanged older resume
test, `persisted_nested_memory_has_identical_hot_and_cold_payload_and_projection`.
It fails with `missing fixed session mode`: its transcript fixture does not
persist the fixed-mode record required by the existing resume path. Both that
test and the failing fixed-mode validation match the base commit. The failure
was reproduced alone and is excluded from the integrated memory command;
these changes do not relax that validation.
