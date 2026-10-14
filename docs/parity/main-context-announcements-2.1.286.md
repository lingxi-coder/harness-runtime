# Main context announcements in 2.1.286

The ordinary main request uses durable `instructions`, `session_context`,
`context_sections`, and `date` attachment rows in that order. The existing
model-reminder side table retains each original payload under its message UUID.
The rendered user-meta message is a projection; an empty native projection uses
an empty System placeholder that contributes no API message or model tokens.
JSONL serializes a single attachment row with `message: null` in either case.

Live and cold announcement baselines come from typed payloads in the active
model history. An orphan side-table entry cannot suppress an announcement after
the corresponding history was compacted away. The instruction descriptors keep
the host loader's order and use native JavaScript whitespace trimming. Branding
in the runtime-owned context trailer uses the product name; file bodies remain
unchanged apart from the native descriptor trim and renderer handling.

Ordinary fallback reuses the prepared attachment identities and bytes. A
successful reactive compaction runs the producer again against its replacement
history with the frozen descriptor snapshot and captured reason. For the normal
auto/PTL preserved-tail transition, missing announcement families are inserted
immediately after the compact boundary and before the summary, under the
`tengu_calm_noodle` gate and matching preserved anchor. Already kept initial
announcement families are not inserted again.

Cold `contextRendering` routing uses the latest raw `prompt_snapshot`, under
the independent default-enabled `tengu_foamy_spring` gate. An invalid latest
snapshot or missing/invalid optional hint selects announced routing; it does not
fall back to an older inline hint. The existing static-prefix recovery still
selects its last valid snapshot independently. A named `context_rendering_hint`
slot carries that raw routing result through main cold and hot resume, while the
existing `prompt_snapshot_resume` flag distinguishes restored metadata from
live recording. Clear resets both. Inline routing retains its outgoing-only
user-context prefix.

Source evidence is pinned to SHA-256
`75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
In `src_191662485.js`, Or is captured before the outer query loop; the loop runs
autocompaction before Jn/yi. Reactive compaction returns to that outer loop;
ordinary fallback stays inside the later model-attempt loop. Gr/Cn place missing
rows after the boundary. In `src_183727495.js`, H_o/FFn/O_o reconstruct typed
instruction deltas; fmn/xd validate the latest raw routing snapshot.
`scripts/tests/instruction_announcements_286_oracle.mjs` executes those narrow
native helpers and records the fixture consumed by core and request tests.

The Gv production port now retains one root/session context build and its
eager-file projections across model steps. The separate qb cache responds only
to its explicit invalidation events; arbitrary filesystem edits do not refresh
Gv. Main preparation retains its original load handle, inline envelope and
refresh reason before compaction. Directory-added, compaction and clear use the
actual lifecycle seams. The Runtime account observer performs pT without WC and
reads current first-party account identity. Child Full/ManagedOnly now project
the same current root Gv through a weak bound provider; the old raw provider
factory has been removed without a compatibility alias.

The cache's native oracle passed 20 scenarios and 300 operations. Core's 5
cache regressions passed within its 743-test full run, and Orchestrator's 16
cache regressions passed within the current 1,277/1,277 full run. The Agent
full run passed 595/595, including authoritative Full-load failure and current
SDK contract coverage. Initial Gv acquisition propagates failure before request
dispatch. The required current capabilities and explicit missing-backend
failures are documented in
[`current-only-sdk-contracts.md`](current-only-sdk-contracts.md).

Native policy-helper refresh, SettingsSync, trusted plugin HooksInvalidate,
live memory controls and host-owned health/acquisition error boundaries remain
gaps. Conditional fresh discovery now has a required provider capability and
no main rule cache: live-cwd conditional-only walks retain shared sent-path
dedup while leaving eager Gv/qb untouched. Its four filesystem/reminder and two
helper regressions passed within the current Orchestrator full run. The memory
library passed 196/196, and the separately persisted native memory helper oracle
passed 27 cases (20 byte, 3 stat and 4 backend). It executes l0/QDn/Hot with
controlled Node fs/Buffer, backend and parser seams; it does not establish
native CLI/Bun or host health acceptance. The earlier 12 LMe/Hot soft-skip
probes remain ad hoc evidence, not additional persisted coverage. Full's Vec
return does not itself establish a rejection mismatch.

Related suites passed sidequery 60/60 and compaction 275/275. Current integration
filters passed force_compact_real 17, memory_seed 2 and nested_discovery 8.
The compact_persistence rerun passed all four tests. It checks the exact
context/date/token-budget attachment suffix, physical order, serialized payload
bytes, hot UUID projections and the complete cold-resume parent chain. The
Runtime pool-starvation integration passed all three tests using actual model
entry, pending futures and cancellation-drop receipts.

The host git seam provides the existing start-of-conversation snapshot;
attached-project and Perforce acquisition remain absent. See
[`instruction-cache-2.1.286.md`](instruction-cache-2.1.286.md) for the independent
cache lifecycle, event atomicity, source ranges and execution boundaries.

The native transient
`resumeIncompleteThinking` branch introduced by max-output-token recovery is not
represented in the current main loop, so its announcement suppression is outside
this implemented ordinary lane. Native bare-fork routing is distinct from a
CLI customization `--bare` option; no equivalence is inferred here.
