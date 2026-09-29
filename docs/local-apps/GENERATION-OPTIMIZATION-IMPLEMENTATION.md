# Local App generation optimization implementation

Approved baseline: `21771b43a`, 2026-09-05. This is the execution ledger for the
user-approved quality, latency, and isolation plan. No general Plugin/MCP/Agent/
Workflow protocol changes, dependency additions, or runtime-profile version edits.

## Second review repair pass — 2026-09-06

Review baseline: `f81c57261`. This pass addresses the twelve newly confirmed
integration defects; the earlier green counts below do not verify these paths.
Implementation uses bounded Luna xhigh lanes and independent Sol xhigh review.
No general Plugin/MCP contract, UniFFI field, runtime-profile version, or dependency
change was made. Existing unrelated work and published-build history are retained.

Repair plan (regression first, then the smallest owning-layer change):

- [x] R2-1: carry safe capture/evidence identity through actual model image output.
- [x] R2-2: accept authenticated failed QA candidates without requiring the broken
  operation to succeed; keep passing evidence gates strict.
- [x] R2-3: expose the durable unresolved-finding ledger to each fresh tester.
- [x] R2-4: bind native QA to Host-observed current-device targets and report the
  remaining design targets/scenarios as unverified.
- [x] R2-5: derive query causality from matching recorded writes and returned rows.
- [x] R2-6: prevent non-navigation evidence from being retagged after navigation.
- [x] R2-7: fence Android captures on the current visual state and rendered frame.
- [x] R2-8: handle same-document Android navigation without losing QA readiness.
- [x] R2-9: preserve native operation errors inside authenticated failure evidence.
- [x] R2-10: validate and consume the exact authoring candidate under the build lock.
- [x] R2-11: validate all referenced artifact bytes before sealing QA results.
- [x] R2-12: prevent trusted-success task output after QA publication fails, even
  if writing the failure spool also fails.

Cross-layer decisions: the full product/design target matrix remains unchanged.
Only Host device context determines this run's native verification scope. A scoped
pass must retain explicit unverified targets/scenarios and cannot become a global
UI-passed summary. A run with no matching device target is unavailable, not an
empty success. Failed native operations may be recorded only with valid native
identity; transport/identity failures are not authenticated UI evidence. Neither
kind of failure is successful persistence proof.

### Implementation and verification

The owning changes are in `local-apps/src/{qa,authoring}.rs`, mobile Host/build/tool
projection, the two Local App workflows and QA guides, both native WebView
controllers, and the existing Local App terminal adapter in `tasks`. The ordinary
task/output protocol is unchanged. No dependency manifest, lockfile, Runtime
Profile bundle, general Plugin/MCP contract, or UniFFI DTO was changed.

Sol's independent review tightened three additional boundaries: failed Canvas or
mixed passed/failed scenarios can produce failed candidates without success-only
proof; every referenced artifact kind is integrity-checked; and Host QA publication
is the terminal commit point. A failed publication never exposes canonical success.
A successful publication followed by a spool-write failure retains the canonical
result through TaskOutput/notifications and suppresses the stale physical path.
Normal tasks and cancellation retain their existing behavior.

Integration follow-ups preserve ordinary Android captures on API 26–28 while
requiring a fresh, tokenized visual-state/frame-commit fence for each QA capture.
Routed/fragment captures validate against canonical Host identity and retain the
exact observed route. Both clients now require navigation advancement only for
accepted navigation: failed Back/rejected Navigate preserve their original error
only when pre/post document identity is unchanged.
Use-test retains caller-requested scope/scenarios in both initial and resample
agent prompts, as advisory intent subordinate to Host-required coverage. The unused
use-test summary helper was removed. The QA skills now explicitly distinguish
diagnostic failure evidence from success proof and current-device coverage from
full-platform coverage; the checked root mirrors remain byte-identical.

Current verification evidence, 2026-09-06:

| Check | Result |
|---|---|
| `cargo +stable test -p engine-mobile --features uniffi --lib --locked`, four test threads | 721 passed |
| `local-apps`, including integration fixtures | 301 unit + 5 integration passed |
| `tasks`, including integration tests | 366 unit + 1 integration passed |
| Executed Local App workflow scripts | 38 passed |
| `skill-api --lib` | 38 passed |
| Workspace `--all-targets --locked` check | Passed |
| Scoped Local Apps/tasks/mobile Clippy | Completed with warnings, no errors |
| Plugin Phase 2/6/7 and optimization guards | Passed |
| Six JSON metaschemas and QA positive/empty-scope negative fixtures | Passed |
| `quick_validate.py`, changed frontend-qa and local-app-test skills | Passed using the existing Anaconda Python environment |
| Android LocalAppWebViewTest / LocalAppsContractTest | 43 + 11 JVM tests passed |
| iOS three selected QA helper tests in simulator | 3 passed; not WebView end-to-end tests |

Verification limits and unrelated gates retained:

- Android `lintDirectDebug` reports 73 errors outside `LocalAppWebView.kt` (first:
  `ConversationSource.kt` uses an API 30 method below its API level). The changed
  WebView file has no lint errors. This is not a green full-project lint result.
- Full iOS test compilation remains blocked by the unrelated
  `BackgroundTasksPanelTests.swift` `stage:` error. Only the selected helper tests
  were run with `EXCLUDED_SOURCE_FILE_NAMES=BackgroundTasksPanelTests.swift`; no
  source or project configuration was changed to bypass it.
- `clients/translations/generate.py --check` still reports drift in the existing
  iOS catalog and five Android generated locale files. Translation sources and
  generated resources were not changed by this pass.
- No physical-device create/approval/Canvas WebView end-to-end run was performed.
  JVM lifecycle tests and simulator envelope/state helpers do not establish real
  compositor timing or multi-device visual acceptance.
- These checks do not measure on-device latency. Guide-size gates remain structural
  source-byte measurements, not a claimed runtime speed-up.

The final Sol closeout independently confirmed routed Android capture, failed
Android navigation, and failed iOS navigation are fixed in their production
callers, with no remaining scoped blocker. It retained the native-test limitations
above. All changed Rust files pass targeted rustfmt checks; `git diff --check` is
clean. No commit or merge was performed.

## Review repair pass — 2026-09-06

The earlier suite counts below are historical measurements, not proof that the
reviewed Host/tool/workflow boundaries work together. All fourteen review fixes
now have passing regression coverage and independent Sol xhigh review. The final
integrated evidence for this repair pass is recorded separately below.

Repair/cleanup plan (preserve unrelated work; no new dependencies):

- [x] R1–R3: use one real opaque contract/QA handle format, carry build run IDs,
  and inject the active build-selected authoring digest into workflow context.
- [x] R4: return Host-recorded evidence IDs from collection operations so the
  read-only tester can retrieve actual artifacts without guessing or filesystem access.
- [x] R5–R6: preserve normal page mutation responses and read-after-write behavior;
  attribute evidence separately, with cancellation-safe action-window cleanup.
- [x] R7: accept same-runtime business routes while retaining origin/generation checks.
- [x] R8: preserve failed scenario judgements and nested findings across finalizers;
  resolving a failure requires new, matching Host evidence.
- [x] R9: mint and restore authenticated use-test scope, including terminal QA checks.
- [x] R10: restore MCP promotion via a narrowly authorized role, without granting
  promotion or mutation to the build/use-test verifier.
- [x] R11–R12: derive UI-update design impact from old/new Host-validated specs;
  omit unasked MCP intent rather than sending a schema-invalid null.
- [x] R13: reclaim every Host-owned QA session for a terminal app/run, including
  failures, cancellation and superseded resample/repair sessions.
- [x] R14: Android navigation preserves only the reserved runtime marker from the
  old page, alongside parameters explicitly supplied by the new destination.
- [x] R15: align the pre-existing MCP authoring workflow/schema with the actual
  camelCase `AppMcpProposal` Host DTO and snake_case Flow binding variant tags,
  and accept the Host-injected top-level run/profile/collection context.

Use failing behavioral regressions before their fixes where practical. Verify real
Host output against the next tool/agent schema, not manually fabricated IDs.
Run focused core/mobile/workflow/native tests first, then integration/static checks.
Luna xhigh implements bounded slices; Sol xhigh independently reviews and fixes.

### Repair-pass implementation and evidence

The repairs are concentrated in `local-apps/src/{ids,authoring,qa}.rs`, the mobile
Host QA module and workflow/tool boundaries, Local App Plugin agents/workflows/
schemas, and Android's Local App navigation controller. Generic Plugin/MCP
protocols, Runtime Profile versions, dependency manifests/locks, and unrelated
client behavior remain unchanged. No commit or merge was performed.

The restored MCP authoring path also needed narrowly related contract corrections:
the checked-in proposal schema now matches the actual camelCase Rust proposal DTO
(binding variant tags remain snake_case), the script accepts Host-injected internal
fields, and the dedicated promoter passes only real Host tool arguments. This is
a Local App contract alignment, not a general MCP protocol change.

The key simplification is that QA no longer buffers business database writes.
Normal writes/results/errors remain real; only their attribution as UI evidence
waits for native confirmation. Evidence IDs are returned by Host collection calls,
not invented by an operator. Terminal cleanup is keyed by authenticated app/run,
not by a model's final JSON. Scenario failure IDs use a collision-safe private
Host namespace, and fresh matching evidence is required to clear them.

Final stable-tree verification, 2026-09-06:

| Check | Result |
|---|---|
| `engine-mobile --features uniffi --lib`, four test threads | 713 passed |
| `local-apps`, including integration fixtures | 290 unit + 5 integration passed |
| Executed Local App workflow scripts | 35 passed |
| `tasks --lib` | 363 passed |
| `permission --lib` | 1,420 passed |
| Real `skill-api` parser | 38 passed |
| Android DirectDebug `LocalAppWebViewTest` | 36 passed, zero skipped |
| Workspace `cargo check --workspace --all-targets --locked -j 4` | passed |
| Core and UniFFI mobile Clippy, all targets | passed with existing warnings |
| Targeted Rust formatting and `git diff --check` | passed |
| Plugin phase 2/6/7 and optimization guards | passed |
| Draft 2020-12 schemas, real fixtures, invalid-handle/proposal negatives | passed |

Schema validation used the already installed `/opt/anaconda3/bin/python` and
`jsonschema.Draft202012Validator`; no dependency was installed or added to the
project. The small `local_apps::value_matches_schema` helper is deliberately not
claimed as full JSON Schema validation: it does not enforce patterns or conditional
keywords. New regressions distinguish structural schema assertions from actual
Host behavior and include real launcher-to-terminal and native-response seams.

Remaining verification limits: native responses in Host tests are simulated;
the Android suite is JVM-based, and no physical-device creation/approval/promotion
end-to-end run or latency benchmark was performed in this repair pass. Previously
documented unrelated translation-generation and iOS full-suite limitations remain
unchanged; the historical verification ledger below is not a new claim that those
gates passed.

## Delivery order

- [x] 1. Baseline measurements and behavior regressions.
- [x] 2. Authoring contract, UI confirmation, persistence, and MCP capability intent.
- [x] 3. Host QA evidence, independent evidence reading, and completion validation.
- [x] 4. Plugin orchestration and selective renderer loading (5/6/7 first-pass calls).
- [x] 5. Mobile session tool/skill scope and disabled/on-demand startup.
- [x] 6a. Dependency cache/lock improvements and device-visible timing diagnostics.
- [ ] 6b. Fixed-model physical-device cold/warm end-to-end performance comparison.

Each delivery is reviewed independently with Sol xhigh before commit. Implementation
uses Luna xhigh. No bulk formatting or unrelated cleanup. Existing failing cases
are protected before their corresponding implementation changes.

## Shared interface decisions

New model-facing authoring/QA JSON uses snake_case. The existing
`AppMcpProposal` Rust DTO remains camelCase at its Host boundary, including
camelCase proposal/tool fields and snake_case externally tagged Flow bindings.
Existing persisted AppManifest, AppRecord, runtime profile bindings, and public
client DTO conventions are retained.

`AppAuthoringSpec` is model-authored, closed, and contains:

```json
{
  "product": {"goal": "...", "tasks": ["..."], "external_integrations": []},
  "targets": [{"id": "primary", "os": "ios", "form_factor": "iphone"}],
  "ui": {
    "structure": ["..."],
    "theme": {"mode": "system", "accent": "..."},
    "style": {"direction": "...", "density": "comfortable"},
    "references": []
  },
  "design": {
    "presentations": [{"target_id": "primary", "presentation": "...", "navigation": "..."}],
    "tokens": {"accent": "..."},
    "states": {"loading": "...", "empty": "...", "error": "...", "success": "...", "permission": "..."},
    "inputs": {"pointer_touch": ["..."], "keyboard_mouse": ["..."], "back": "...", "reduced_motion": "..."},
    "canvas": null
  },
  "acceptance_checks": [{
    "id": "primary-action", "target_ids": ["primary"], "required": true,
    "preconditions": ["..."], "steps": ["..."], "expected": "...",
    "evidence": ["inspect", "ui_action", "capture"]
  }]
}
```

The Local App build workflow receives the complete object as `authoring_spec`
for both Create and Update. A free-form `revision_prompt` may explain an Update,
but cannot substitute for its confirmed intent or acceptance criteria. Verify and
use-test read the effective Host contract and reject caller-supplied replacements.
There is no legacy free-form specification fallback in this unpublished flow.

Canvas design additionally names scene, phases, controls and HUD. Renderer/profile
identity is never accepted inside the authoring spec. Host wraps the spec in
`AppAuthoringContract { version, revision, app_id, runtime_profile, spec }` and hashes
canonical bytes. The approved plan is the only source of the product, targets, UI
intent and acceptance requirements, so those cannot be silently rewritten.

`LocalAppContract` accepts `operation=get|stage`, `app_id`, and for staging
`workflow_run_id`, `spec`, optional `base_contract_sha256`, and the existing
`validated_selection_handle` for create. Host returns `contract_handle`,
`contract_sha256`, `contract`, and non-authoritative display summaries. Private
candidate/contract files live outside workspace under `apps/<id>/authoring/`.
The effective authoring digest is read from successful build provenance; failure
never changes the old build's contract. Direct builds without a new handle retain
their already committed contract. A generated build without any contract cannot
be reported UI-verified.

New QA operations are `LocalAppQaBegin`, `LocalAppQaReadEvidence`, and
`LocalAppQaFinalize`, through existing first-party tool registration/dispatch.
Begin binds app/run/build/profile/dependency/authoring/manifest/runtime generation
and re-reads required collections and scenario IDs from Host state. UI/data inputs
may attach `qa_handle`, `scenario_id`, `target_id`; these must be checked against
Host bindings, never trusted as proof of the observed native target.

ReadEvidence returns actual recorded JSON/image content blocks, not base64 text.
Raw evidence lives in bounded per-run temporary storage; durable results retain
hashes/metadata, findings, and scenario coverage. Direct test-data mutation does
not count as UI persistence. Finalize accepts scenario judgements and structured
findings, validates actual Host evidence, and stores a candidate result. Mobile
terminal handling revalidates a claimed success before publication. Historical
verification can describe its immutable build after stop/restart; in-flight QA
evidence cannot be reused across runtime generations or workflow runs.

Fast/balanced: selector, optional designer, create-preparer, builder, operator,
tester. Thorough adds verifier. Preparer has no file/build tools and combines
stage+native approval through existing calls; no new workflow primitive. Tester
and verifier read evidence only. They cannot erase unresolved upstream failures.

Global Code tools are exactly Create/List/Get; app workspaces retain all scoped
operations. Dynamic business MCP remains unchanged. Global Local App skill listing
contains only create-local-app/local-app-use/expose-as-mcp; the live skill registry
and preload semantics remain intact. Disabled plugin boot uses compiled metadata
without materialization and enables through the existing registration path.

## Verification ledger

Baseline known from review: workflow tests 11/11; successful create calls fast=7,
balanced=8, thorough=8; balanced one repair=12. Builder preloaded skill/reference
source bytes=40,520; designer=60,095. Ordinary Code registers 34 Local App operations
(33 eager). These are structural baselines, not device-latency evidence.

Targets: fast/balanced/thorough first-pass calls=5/6/7; builder/designer injected
guide bytes decrease at least 40%; global operation inventory=3. Native QA failures,
wrong/stale identities, empty coverage, upstream-failure erasure, and direct data
mutation masquerading as UI roundtrip must fail closed. Record test commands and
device-measurement availability below as work proceeds.

### Independently reviewed invariants

These boundaries have focused executable regression coverage:

- Persist build receipt inside staging before promotion; an I/O failure must not
  select a new build or authoring contract.
- Require scenario-owned, target-matched evidence; stateless apps do not need a
  database round-trip, and identical reduced-motion frames are not a failure by
  themselves.
- Tester finalization anchors findings in the Host ledger even on thorough runs.
  Verifier reads the same artifacts before terminal cleanup.
- Keep immutable QA result hashes stable; only the terminal adapter can publish
  a successful candidate. Task registry, output spool, UI status, and notifications
  must agree, including cancellation and publication failures.
- Scope Local App tools using the Host data root, not a coincidental directory
  suffix. Initial App-scoped engine startup may activate its one current App, but
  ordinary startup must not reconcile every App.
- Mobile Rust validation uses `--features uniffi`; featureless checks do not
  compile the changed native Host implementation.
- The final commit checks current runtime generation under a per-App identity
  guard. Prepare-then-restart cannot publish stale in-flight evidence; an already
  published result remains history for its own immutable build.
- Native capture evidence is decoded from the actual nested `image.data`
  response, not a model frame counter or a synthetic top-level image field.
- UI QA has separate required/passed/corrupt codes. It cannot inherit the
  existing nil-code MCP success sentence on iOS or Android.
- MCP promotion runs through a dedicated tools-only role. It passes only the
  real Host tool inputs (`app_id`, `workflow_run_id`, optional non-null
  `receipt_id`) and normalizes the Host response to a closed result schema;
  tester/verifier retain no promotion authority.
- The shell is a thin identity/no-write guard and immediately hands off to the
  create skill. It no longer runs a second questionnaire, forces a DOM/Canvas
  choice, or describes MCP exposure as an external-service setup prerequisite.
- Successful builds, dependency rebuilds and checkpoint rebuilds refresh this
  App's verification summary even when no MCP catalog exists. A changed build
  clears a cached Passed badge; a failed build preserves its old build/receipt.

### Available validation environments

- Independent native review completed: Android focused WebView tests passed
  31/31, iOS QA/controller tests passed 7/7 (including composited capture), and
  Rust runtime URL tests passed 2/2 with `--features uniffi`. The final integrated
  tree still needs a rerun after the Host/core reviews. No Android device is
  currently attached.
- An iPhone 17 Pro simulator is booted, and a paired iPhone 11 is available.
  Initial targeted XCTest execution was blocked before running by an unrelated
  `BackgroundTasksPanelTests.swift` compile error. The successful diagnostic run
  excluded that file using `EXCLUDED_SOURCE_FILE_NAMES`; it was not a full-suite
  pass and was simulator evidence, not a physical-device performance benchmark.
- The bundled Python runtime lacks PyYAML, so `quick_validate.py` is not yet a
  passed check. Use the real project skill parser and report this tool limitation
  separately; do not count an equivalent parser as the original script passing.
- Root verified `cargo test -p tasks output_manager::tests --locked`: 8/8,
  including unrelated-spool concurrency and late raw-output suppression.
- Root reran the complete `tasks` library suite: 363/363 passed. The complete
  `local-apps` suite passed 281 library tests and 5 integration tests, and the
  real `skill-api` parser suite passed 38/38.
- Final native-source reruns passed Android `LocalAppWebViewTest` 31/31 and
  the seven selected iOS QA/controller tests 7/7. The same pre-existing iOS
  test-file exclusion described above remains necessary. These tests do not
  constitute a rebuilt native Rust framework or a complete live generation run.
- Phase 2, Phase 6 and Phase 7 Plugin guard scripts pass. Phase 2 verifies
  27 skills, 9 agents, 3 workflows, 6 schemas and the exact compiled inventory.
- The focused QuickJS Plugin workflow suite passes 35/35. Its callbacks now
  reuse the checked-in Host-valid AuthoringSpec and QA-result fixtures, exercise
  `contract_<32>`/`qa_<32>` identities, and fail closed on a permissive runner's
  fabricated QA handle. The real Draft 2020-12 validator accepts the populated
  and zero-tool MCP proposal shapes and rejects the stale snake_case form.
- Translation `generate.py --check` still reports a pre-existing mismatch in
  iOS and all five Android catalogs: the canonical compaction copy differs and
  five still-referenced compaction keys are missing from canonical sources.
  Blind full regeneration removes those resources and breaks Android. This task
  adds only three UI QA keys: all 3 × 5 locale values exactly match canonical
  JSON, iOS and Android. Unrelated generated content is preserved.
- The translation generator's 16 tests passed during native review, but tests
  at `test_generate.py:51/63` override only one output directory and rewrite
  the other platform's real catalogs. The scoped resource diff was restored
  afterward; do not interpret that test run as a clean repository-wide
  translation-generation gate.

### Measured structural result

| Measurement | Baseline | Optimized | Evidence |
|---|---:|---:|---|
| Fast first-pass create agent calls | 7 | 5 | Executed QuickJS workflow |
| Balanced first-pass create agent calls | 8 | 6 | Executed QuickJS workflow |
| Thorough first-pass create agent calls | 8 | 7 | Executed QuickJS workflow |
| Builder injected guide bytes | 40,520 | 14,159 (−65.06%) | Real MobileDiskSkillLoader |
| Designer injected guide bytes | 60,095 | 26,375 (−56.11%) | Real MobileDiskSkillLoader |
| Ordinary Code Local App operations | 34 | 3 | Exact scoped inventory tests |

The first-pass workflow tests invoke the scripts, rather than trusting their
reported counters. The 35-case workflow suite also covers Update/Verify/Repair,
upstream findings, native refusal, separate evidence resampling, and source-only
repair budgets. QA prompts exclude the schema catalog and selector capability;
repair receives the Host QA profile explicitly even though discovery/mutation
tools are denied in that role.

The full mobile supply-chain regression and direct pin verifier pass without
changing any protected package/lock/WASM/profile bytes. Its existing warning
about an unanchored release-rootfs digest remains a separate supply-chain gap.
The baseline brand-leak scanner also remains non-green on unchanged unrelated
paths; this task does not replace the global branding baseline.

### Regression commands

Run from `lingxi-code/` unless another working directory is named:

```sh
cargo check --workspace --all-targets --locked -j 4
cargo test -p engine-mobile --features uniffi --lib --locked --no-fail-fast -- --test-threads=4
cargo test -p local-apps --locked --no-fail-fast
cargo test -p tasks --lib --locked --no-fail-fast
cargo test -p permission --lib --locked --no-fail-fast
cargo test -p skill-api --lib --locked
cargo test -p workflow --test plugin_workflow_scripts --locked
```

Root-observed runs passed the complete workspace check; final mobile 697/697;
core 281 + 5; tasks 363; permissions 1,420; skill API 38; and workflow scripts 30.
Clippy passed for the six affected Rust packages and all their targets (with
existing warnings, not a `-D warnings` claim); targeted `rustfmt --check` with
`skip_children=true` and `git diff --check` passed. Android DirectDebug's
combined WebView and verification-copy run passed 31 + 11 tests; PlayDebug's
verification-copy suite also passed. The iOS seven WebView tests and the focused
verification-copy test passed with the documented unrelated test-file exclusion.

Unrestricted parallel mobile sweeps intermittently failed two timing-sensitive
tests outside the changed behavior: pause-state observation and MCP reload/revert.
Both passed isolated reruns. The final complete 697-test run passed with four
test threads, without skipping tests or changing those generic control paths.
This documents test scheduling sensitivity rather than claiming it was repaired.

Additional checks from the repository root:

```sh
python3 lingxi-code/scripts/checks/check-phase2-plugin.py
python3 lingxi-code/scripts/checks/check-phase6-plugin.py
python3 lingxi-code/scripts/checks/check-phase7-plugin.py
bash lingxi-code/scripts/tests/test-local-app-supply-chain.sh
python3 clients/translations/generate.py --check
git diff --check
```

The translation command is deliberately still a failing baseline gate, as
explained above. The optimization source-proxy script was retired with the build
workflow it measured; the authoritative Rust loader regression remains and does
not need to fetch repository history.

## Narrow shared-task integration

No generic Workflow protocol or default completion semantics changed. There are
two additive internal task helpers because a Local App success has to publish a
checked result and a Host QA receipt without racing cancellation:

- `TaskOutputManager::replace_terminal_result` replaces this task's raw terminal
  spool with canonical JSON and suppresses later raw appends; its lock is per
  spool, not shared across unrelated task output.
- `TaskRegistry::commit_local_app_workflow_terminal` accepts only authenticated
  Local App Build/UseTest scopes. Expensive validation runs before the registry
  lock; bounded local spool/receipt publication and the terminal outcome commit
  happen together. Notifications and artifact cleanup run after that lock.

Ordinary workflows keep their existing path. A logical `ok:false` remains a
Completed task with a failed verification result; a forged success or a failed
publication cannot become a verified Completed result. Build previewability and
UI verification remain separate.

## Device performance collection

Mobile has no installed tracing subscriber, so tracing spans alone cannot prove
device timings. `LINGXI_LOCAL_APP_PERF_DIAGNOSTIC=1` enables Local-App-only stderr
events in the form `[local-app-perf] phase=<fixed-name> elapsed_us=<integer>`.
The default is silent. Events contain no App names, prompts, package names,
source text, user paths, or credentials. Native approval wait is recorded
separately from machine resolve/install/build/QA and lock waits. Existing workflow
progress records already carry agent/tool counts, tokens and timestamps; use
those records for the LLM portion instead of changing the shared protocol.

Use an identical model/configuration, device, build type and confirmed brief on
the baseline and this branch. Run a DOM CRUD task, a Canvas 2D phase/input task,
and a Three.js resize/lifecycle task. Separate a cold dependency cache run from
warm exact-lock runs. Record the active profile/contract and cache state as
benchmark metadata, not as timing-event user data. Report at least:

- user confirmation waiting time and machine elapsed time separately;
- executed agent/tool calls and input tokens;
- first preview, QA, resolve/frozen install, SBOM/inventory, and lock waits;
- correctness outcome, evidence resampling and source repair counts.

The automated workflow call-count and loader-byte assertions are deterministic
structural measurements. They are not a percentage claim about end-to-end device
speed. A physical Android device is not attached, and the fixed-model physical
device cold/warm generation comparison is not yet available.
