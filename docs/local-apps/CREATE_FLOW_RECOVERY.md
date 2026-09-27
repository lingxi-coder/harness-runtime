# Local App create and modify recovery

Creating a Local App is no longer a Host-verified workflow. The model converges
the requirements and a complete UI recommendation with the user in ordinary PLAN
MODE, the user's Allow on the plan IS the create confirmation, and the Host's
`LocalAppPrepare` lands the approved template before any source is written.
Modify runs the same plan through the same call. Nothing here adds a general JSON
parser fallback, a new serialization format or a dependency.

## Execution and recovery

- The durable approval is the user's, not the model's. The engine's own success
  branch of `ExitPlanMode` is the single writer of "this plan was approved";
  `plan_approval.rs` observes it as a transparent listener decorator and records
  it keyed by plan-file path, bound to the conversation that approved it, with
  the digest of the approved plan text. A plan approved in one conversation can
  never be spent in another, and a subagent plan is never a user approval.
- `LocalAppPrepare({"app_id","plan_path"})` is the whole create/modify step, and
  `plan_path` is the only part of it the model supplies. The transport rebuilds
  the request from its own approval record, so `name`, `brief`, `spec` and
  `template_id` cannot be forged into the call. Before anything is landed,
  `verify_plan_unchanged` re-reads the plan file and refuses if the bytes differ
  from the approved ones, and the approved `template_id` is re-validated against
  the LIVE catalog — a template that moved is refused by name (`template_stale`)
  rather than silently swapped for a similar one.
- CREATE (an app that is still an empty shell) lands the template through the
  existing scaffold transaction: journaled selection, staged authoring contract,
  isolated create staging, one-shot receipt, then the reservation/rollback path
  in `scaffold_shell_app_value`. MODIFY (an app already built) stages the new
  authoring contract and nothing else.
- Preparation is durable, so an interruption does not lose it.
  `apps/<id>/prepare/state.json` records the execution id, plan path and digest,
  contract handle/digest, resolved runtime profile and the spec, under its own
  payload digest; `apps/<id>/prepare/scaffold-commit.json` is the commit proof,
  written from inside the scaffold transaction BEFORE the record flips
  `scaffolded`. The proof and the app's own `.lingxi/app.json` mirror must agree,
  which is what makes a crash between the seeded workspace and the record commit
  recoverable on the next call instead of ambiguous.
- `LocalAppPrepare` is idempotent for its purpose. A repeated call for the same
  approved plan reuses the prepared result — the same execution id and contract
  handle — and never overwrites source that was already written, so retrying
  after a failure is safe. A different plan for the same app is refused
  (`prepare_rejected`) rather than re-prepared, and after the scaffold commit the
  record is returned as already prepared.
- A create whose template never landed is RE-PLANNED through plan approval, not
  resumed. The retired create coordinator's recovery sidecars
  (`create-input.json` / `create-terminal.json`) are gone: nothing in production
  reads them, and the failed-create resume capability they backed no longer
  exists. Recover by planning again and letting the user approve.
- A failed modify keeps the last successful build and its valid authoring
  contract exactly as they were. The staged contract is only promoted by a build
  that succeeds.
- After preparation, work continues from the current workspace with the ordinary
  tools — manifest, source, `LocalAppBuild` — and a successful build is the
  completion condition. The preview is started or restarted with
  `LocalAppRuntime`, and delivery is the entry point plus the trial checklist
  with status `已构建，待你试用`. An app whose build already succeeded is
  delivered as it stands rather than pushed through any retired automatic QA
  stage.
- `ResumeWorkflow` still recovers a workflow whose session `adopt.json`
  checkpoint survives an evicted registry row, and it still adopts only a
  VERBATIM plugin workflow: provenance comes from the registry's own script
  bytes, never from the caller's `meta.name`. `lingxi-local-app:local-app-use-test`
  and `lingxi-local-app:local-app-mcp-authoring` are the live plugin workflows.
  What a resume can no longer do is continue a create — the retired coordinator's
  recovery sidecars went with it, so a create whose template never landed goes
  back through planning.
- Build progress is recorded where it always was: `apps/<id>/build/build.json`
  holds the build id, build key, runtime-contract digest, dependency-snapshot
  digest, output digest and — when a contract was staged — the authoring-contract
  digest, with dependency state in `dependencies.json` and the bounded log under
  `apps/<id>/logs/`.
- Refusals name the remedy because the caller's only correct response is to send
  the user back through planning: `plan_approval_missing` (nothing approved in
  this conversation names that plan file), `plan_approval_spent` (that plan
  already prepared a different app), `plan_approval_invalid` (the authoring block
  could not be honoured), `plan_approval_unavailable` (this host build keeps no
  plan-approval record), `template_stale`, `prepare_rejected`, `prepare_invalid`
  and `prepare_state_invalid`.
- Automatic verification is deliberately NOT retained on create or modify: there
  is no QA scoring and no verification-driven repair loop. The on-demand testing
  capability stays, through the independent `lingxi-local-app:local-app-use-test`
  workflow and the shared QA completion gate that path still uses
  (`qa_begin` / `qa_read_evidence` / `qa_finalize`), and it runs only when the
  user asks for it.

## JSON handling

The plan's machine-readable input is JSON, and it is strictly parsed rather than
patched.

- The plan must carry exactly ONE fenced block whose info string is
  `authoring-spec`. Every other fence is ignored; a second block or an
  unterminated fence fails closed; the block and `AppAuthoringSpec` are both
  `deny_unknown_fields`, so extra keys are refused instead of half-honoured.
- A malformed block is recorded as a REJECTION with its reason, so it is reported
  by name instead of looking like a plan the user never approved. The remedy is to
  fix the plan and have the user approve it again — never to hand-edit around the
  parser.
- `name` and `brief` are bounds-checked at approval (non-empty, within the
  record's byte limits) and the spec is validated there too, so a plan that
  cannot be prepared is refused at the approval it was granted for rather than
  after the approval is spent.
- Identity is never re-read from the model's tool call: `prepare` reads `name`,
  `brief`, `spec` and `template_id` only out of the approval record. There is no
  general JSON parser fallback, no transport response-body patching, and no
  model-authored substitute for any of those four fields.

## Changed areas

- `lingxi-code/apps/engine-mobile/src/local_apps_prepare.rs` — new. Durable
  prepare state and scaffold commit proof; CREATE landing through
  `scaffold_shell_app_value`; MODIFY staging only; reuse on retry.
- `lingxi-code/apps/engine-mobile/src/plan_approval.rs` — new. The Host-owned
  approval record, the closed `authoring-spec` parser, the plan-digest
  re-verification and the one-app spend.
- `lingxi-code/apps/engine-mobile/src/local_apps_mcp.rs`,
  `lingxi-code/apps/engine-mobile/src/local_apps_tools.rs`,
  `lingxi-code/permission/src/defaults_per_tool.rs`,
  `lingxi-code/permission/src/mode_policy.rs` — the `prepare` operation on the
  tool catalog, the `LocalAppPrepare` builtin and its permission default in the
  global tool set, the shell allowlist, and the plan-mode policy that lets a
  planner read the catalogs and change nothing.
- `lingxi-code/apps/engine-mobile/src/local_apps_host.rs`,
  `lingxi-code/apps/engine-mobile/src/local_apps_host/authoring.rs`,
  `lingxi-code/apps/engine-mobile/src/local_apps_build.rs`,
  `lingxi-code/apps/engine-mobile/src/local_app_template_catalog.rs` — create
  approval authority (`create_requires_approved_plan` for a tool-path create),
  the reused scaffold transaction, and the live-catalog re-validation.
- `lingxi-code/apps/engine-mobile/src/workflow_support.rs`,
  `lingxi-code/apps/engine-mobile/src/host.rs`,
  `lingxi-code/apps/engine-mobile/src/lib.rs` — the plan-approval watcher on the
  turn loop, and the parse of workflow adoption/provenance that no longer
  carries a create resume.
- `lingxi-code/plugins/lingxi-local-app/skills/create-local-app/SKILL.md` — the
  new flow, with the root mirror `skills/create-local-app/SKILL.md` kept
  byte-identical.
- iOS `clients/ios/Sources/LocalApps/` (`LocalAppApprovalSheets.swift`,
  `LocalAppsModels.swift`, `LocalAppsStore.swift`,
  `LocalAppsProtocolAdapter.swift`) and `clients/ios/Resources/Localizable.xcstrings`.
- Android `clients/android/app/src/main/java/com/lingxi/code/localapps/`
  (`LocalAppsContract.kt`, `LocalAppsScreen.kt`, `LocalAppsViewModel.kt`) and the
  five `clients/android/app/src/main/res/values*/strings.xml` locales.

## Removed

- The create coordinator module and its tests: native Create stage state,
  approval/Scaffold coordination, and the authoritative build/QA references it
  owned.
- The create-resume path and its recovery sidecars: the failed-Create
  "continue from native" capability, the immutable Create input snapshot and the
  terminal marker. One legacy engine fixture still writes the two sidecar files
  for its own setup; no production path reads or writes them.
- The native create-confirmation sheet and its client plumbing: iOS
  `LocalAppCreateConfirmationSheet` with its prompt model, store resolution and
  protocol-adapter mapping, and Android's `LocalAppCreateApprovalSheet` DTO,
  view-model state and screen. The shared protocol keeps the
  `LocalAppCreateConfirmationRequestDto` DTO, the `create_confirmation_requested`
  event and the `resolve_create_confirmation` command as inert wire surface — no
  producer exists and Android maps the event to a no-op.
- 21 `local_apps_create_confirm_*` translation keys in each of the five Android
  locales (`values`, `values-en`, `values-ja`, `values-ko`, `values-zh-rTW`),
  plus their entries in the iOS string catalog.
- The `canContinueCreating` / "Continue Creating" affordance in the client UI.
- The plugin build workflow
  `lingxi-code/plugins/lingxi-local-app/workflows/local-app-build.js` and the
  role agents it drove — `builder.md`, `designer.md`, `template-selector.md` and
  `create-preparer.md` under `lingxi-code/plugins/lingxi-local-app/agents/` — with
  its template-selection skill
  (`lingxi-code/plugins/lingxi-local-app/skills/template-selection/SKILL.md`)
  and the `design-spec` schema
  (`lingxi-code/plugins/lingxi-local-app/schemas/design-spec.schema.json`).

## Kept on purpose

- The shared QA completion gate. It is not a create or modify stage any more,
  but the independent on-demand use-test path still runs through it, so
  `qa_begin`, `qa_read_evidence`, `qa_finalize` and the operator/tester/verifier
  chain are untouched.
- Already-landed-template resume. Repeating `LocalAppPrepare` for the same
  approved plan returns the prepared result instead of re-landing, and the
  scaffold transaction still refuses a second landing on a formed app.
- Modify-time workflow resume, workflow adoption and plugin-workflow provenance:
  only the create branch of that machinery was retired.
- Templates and existing apps. No template revision, lockfile or app workspace
  was migrated or rewritten for this change.

## Validation

- `cargo test -p engine-mobile --features uniffi --lib -- plan_approval`:
  14 passed, 0 failed.
- `cargo test -p engine-mobile --features uniffi --lib -- local_apps_`: 429
  passed, 1 failed. The failure is
  `local_apps_host::tests::a_first_start_skips_a_port_a_concurrent_start_has_leased`,
  which asserts the exact derived loopback port and saw 26143 where it expected
  26142; rerun on its own
  (`cargo test -p engine-mobile --features uniffi --lib -- local_apps_host::tests::a_first_start_skips_a_port`)
  it passes, 2 passed / 0 failed, so it is a port-contention flake in a parallel
  run rather than a create/modify regression.
- `cargo test -p workflow --test plugin_workflow_scripts`: 15 passed, 0 failed,
  including `every_checked_in_plugin_workflow_passes_the_runtime_validators` for
  the two live plugin workflows.
- The engine-mobile library and test targets compile with the new
  `local_apps_prepare` and `plan_approval` modules; the remaining diagnostics on
  that target are warnings.
- What was NOT run in this session: no iOS or Android build, no simulator or
  device run, and no live-provider end-to-end creation. Nothing here establishes
  a physical-device create, and the client and translation changes are only
  reviewed as diffs.
