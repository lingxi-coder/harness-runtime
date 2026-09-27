---
name: create-local-app
description: Plan a confirmed local app with the user, prepare its workspace from the approved plan, implement it, and deliver it after a successful offline build.
---

# Create or modify a local app

The MAIN session does all of this work. There is no build workflow, no mandatory
template-selection agent, no five-part design panel, no quality tier, and **no
automatic verification**. You converge the requirements and a complete UI
recommendation with the user, the Host prepares the workspace from the plan the
user approved, you implement that plan, and you deliver once the build succeeds.

Specialist skills stay available as **reference for the source you write**:
`$frontend-design`, `$accessibility`, `$react-best-practices`,
`$ionic-react-local-app`, and the drawn-surface specialists
`$canvas-2d-local-app`, `$threejs-local-app`, `$phaser-2d-local-app`,
`$babylon-3d-local-app`. Load them while implementing; none of them is a
mandatory stage and none of them holds a gate.

## Entry

When `LINGXI.md` identifies a local app and its ID, that record and its
app-scoped session already exist. Treat that ID as authoritative. Do not call
`LocalAppList` or `LocalAppGet` to rediscover it, and never call
`LocalAppCreate` again inside it. Use `LocalAppList` only from a global
conversation when no app id is known.

The library's "+" creates an EMPTY SHELL: no name, no brief, no surface, an
empty workspace, and an app-scoped conversation. Its `LINGXI.md` names the app
id and says the app has no shape yet. Settling what the app IS happens in that
conversation — that is the planning step below.

`LocalAppCreate` is for a conversation that is NOT an app's: a global or project
chat where the user asks for an app. It creates the same empty shell and app
session; do the planning in that app session, where `LINGXI.md` is auto-loaded.

Until the plan is approved and prepared, the app has no shape: there is nowhere
to write source and every build, dependency, runtime, log, manifest, UI, data,
checkpoint and background tool refuses it. That refusal is the contract, not a
transient failure.

## 1. Plan

Call `EnterPlanMode` for the app you are creating or modifying. Plan mode is a
READ-only mode: you may read the runtime catalog (`LocalAppRuntimeProfiles`), the
template catalog (`LocalAppTemplateCatalog`) and the existing workspace, and you
must NOT write source, land a template, or build. Mutation tools are denied for
the whole of plan mode; do not try to route around that.

Converge the following, asking only materially unresolved questions with
`AskUserQuestion` — never a question quota, never re-asking an answered
decision, and never leaving an unresolved question in ordinary assistant text:

- **Product** — the goal, the main features, and the success condition.
- **Target devices** — OS and form factor for each. Infer them from the brief
  and the fixed Mobile Runtime Environment reminder (`Host OS`, `Device class`,
  `Execution target`, `Launch mode`) when the brief does not say, and show the
  inference for confirmation.
- **Data and permissions** — which collections the app writes, and which Host
  capabilities (manifest permissions) or external integrations it needs.
- **UI** — a COMPLETE recommendation, presented in full: page structure and
  navigation, layout, light/dark/system theme and accent, style and density, the
  complete states, and how it adapts across the confirmed targets. State it
  concretely (screens, back/navigation semantics, tokens, adaptive behavior)
  rather than gesturing at a direction. This is what the user approves, so it is
  not an afterthought.

Read `LocalAppRuntimeProfiles` to choose the runtime profile and
`LocalAppTemplateCatalog` to choose a `template_id`. Both are technical choices
you make and then SHOW in the plan; the user corrects them if wrong. The profile
picks the surface: `canvas` when the whole interface is one drawn surface that
owns a frame loop (a game, a simulation, a 3D scene, a live visualization), `dom`
for everything assembled from screens, lists and forms. Profile family CANNOT be
changed after the workspace lands — an app that needs another family is created
again.

If a confirmed target is iOS or iPadOS and this change affects the UI, load
`lingxi-local-app:apple-design` exactly once before proposing the UI and apply it
to the iOS/iPadOS output only, never to Android or desktop. Skip it for verify-
or code-only work.

### The plan file and the authoring block

Write the plan to the plan file, then call `ExitPlanMode` to request the user's
approval. **That approval — the Allow on the plan — IS the create confirmation.**
There is no second native create sheet afterwards; asking again is the duplicate
confirmation this flow exists to remove.

The plan must carry exactly ONE machine-readable block, fenced with the info
string `authoring-spec`, holding the inputs the Host will land:

````
```authoring-spec
{"name":"Errand List","brief":"Track errands with a quick add flow","template_id":"react-dom-r4","spec":{"product":{"goal":"Track errands","tasks":["add an errand"],"external_integrations":[]},"targets":[{"id":"phone","os":"ios","form_factor":"iphone"}],"ui":{"structure":["list","editor"],"theme":{"mode":"system","accent":"blue"},"style":{"direction":"clear","density":"comfortable"},"references":[]},"design":{"presentations":[{"target_id":"phone","presentation":"single-column list","navigation":"push editor; back returns to list"}],"tokens":{"spacing":"8px"},"states":{"loading":"skeleton","empty":"prompt to add","error":"retry","success":"show list","permission":"explain if needed"},"inputs":{"pointer_touch":["tap"],"keyboard_mouse":["tab"],"back":"pop editor","reduced_motion":"respect device"}},"acceptance_checks":[{"id":"add-item","target_ids":["phone"],"required":true,"preconditions":[],"steps":["open editor","save errand"],"expected":"errand appears in list","evidence":["inspect","ui_action"]}]}}
```
````

- `name` and `brief` are the user-facing wording. They are read from the PLAN,
  never from the `LocalAppPrepare` call, so what the user reads IS what lands on
  the record.
- `template_id` is required to CREATE. Omit it to MODIFY an app that is already
  built: its runtime profile is already fixed and a modify plan may not swap it.
- `spec` is the existing `AppAuthoringSpec` — `product`, `targets`, `ui`,
  `design`, `acceptance_checks`. `acceptance_checks` is the user's TRIAL
  CHECKLIST; nothing scores it and nothing collects its evidence automatically.
- The block is CLOSED: no keys beyond these, and `AppAuthoringSpec` is itself
  closed. A malformed block is refused by name, so fix it and plan again rather
  than trying to land it.
- Exactly one such block. Every other fence in the plan is ignored; a second
  `authoring-spec` block or an unterminated fence fails closed.

Keep the prose around the block readable: it is what the user reviews. Put the
product goal, the features, the target devices, the data and permissions, and
the full UI recommendation in the plan, with the authoring block carrying the
same decisions in machine-readable form.

## 2. Prepare

After the user approves the plan, call:

```
LocalAppPrepare({"app_id":"app_123","plan_path":"<the plan file path the engine reported when the user approved it>"})
```

The Host re-reads its OWN approval record for that plan file, re-checks that the
plan text has not changed since it was approved, re-validates the approved
`template_id` against the LIVE catalog, and then:

- **CREATE** — lands the template through the existing scaffold transaction. This
  is the one-time workspace preparation. You never scaffold yourself and the user
  is never asked to confirm the create a second time.
- **MODIFY** — stages the new authoring contract and nothing else. No template is
  landed, and the last successful build and its valid contract stay exactly as
  they were until a new build succeeds.

`name`, `brief`, `spec` and `template_id` are read from the approved plan, never
from your call. Pass only `app_id` and `plan_path`; a forged name, brief, spec or
template in the call changes nothing.

It returns an execution id and the authoring-contract handle you build with. If
it refuses — the approved template moved, the plan names no `template_id` for a
create, or the plan changed after approval — take the user back through planning
rather than swapping a template silently or editing the plan to force it through.

A repeated call REUSES the prepared result and never overwrites written source,
so retrying after a failure is safe. If the plan is rejected or edited, plan
again and re-approve: an old spec is never executed.

## 3. Implement per the plan

Write the source the approved plan describes, inside the workspace the prepare
step landed. Re-read `LINGXI.md`: it now carries the app's formal workspace
contract — the editable roots, the host-managed files, the entry points that
exist, and the surface the persisted profile takes. That contract governs
everything after it.

You may read the environment's specialist skills for the source you are writing,
but they are guidance: follow the plan's UI recommendation, and if a material
detail is unresolved, ask with `AskUserQuestion` rather than inventing it.

Declare collections, domains and capabilities with `LocalAppManifest` BEFORE
generated source relies on them. Every collection needs `id`, `name` and
`fields`; every field needs `id`, `label` and `kind`. IDs use lower snake_case
and match `^[a-z][a-z0-9_]{0,63}$`. Optional field keys are `required` and
`enumOptions`; supported kinds are `text`, `long_text`, `integer`, `decimal`,
`boolean`, `date_time`, `enum` and `image_ref`:

```json
{"app_id":"<id>","collections":[{"id":"recognition_results","name":"识别结果","fields":[{"id":"source_image","label":"图片","kind":"image_ref","required":true},{"id":"recognized_text","label":"识别结果","kind":"long_text","required":true}]}]}
```

Never declare host-owned record metadata (`recordId`, `revision`, `createdAtMs`,
`updatedAtMs`) as fields. If manifest validation fails, repair the payload and
retry before building; a failed manifest update is not a completed step.

Capabilities are a closed enum: `data_mutation`, `ui_control`, `camera`,
`photo_library`, `microphone`, `location`, `notifications`, `clipboard`, `share`,
`text_to_speech`, `files_read`, `files_write`, `device_status`, `haptics`,
`deep_link`, `calendar`, `contacts`, `media`, `llm`, `agent_notify`,
`background_schedule`. There is no `data` capability. `background_schedule` is
required when the app registers a system background flow. WebAssembly and Web
Workers need no capability — the served policy already allows wasm compilation
and `blob:` workers — so never invent one. `data_mutation` authorizes the
conversation agent to call `LocalAppMutateData`; do not declare it merely because
the page writes its own collection through `window.lingxi.v2.data.mutate`.

Use only `window.lingxi.v2` for host data, network, device and agent events, and
prefer the locked bridge helpers over hand-built payloads:

```js
import {
  deleteRecord,
  queryCollection,
  upsertRecord,
} from "../lib/lingxi-bridge";

await upsertRecord("high_scores", "best", { score: 42 });
const page = await queryCollection({
  collection: "high_scores",
  filters: [{ fieldId: "score", operator: "greater_than", value: 10 }],
});
const scores = page.records.map((record) => record.document.score);
await deleteRecord("high_scores", "best", page.records[0].revision);
```

Collection results expose app fields only under `records[].document`.
`localStorage`, IndexedDB or React state may be a cache but must never be the
authority for a declared collection. Surface native bridge failures as a
retryable error; never swallow them and report a successful save.

Prefer what the host already provides before proposing an external API or a new
package: `requestLlmChat`/`streamLlmChat` plus `onLlmStreamFrame` for the user's
own configured model (never embed provider keys or a direct SDK call), and
`window.lingxi.v2.data`, `device`, `clipboard`, `files`, `network`, `runtime`,
`agent` through the checked-in helpers (`getClipboardText`, `setClipboardText`,
`shareContent`, `synthesizeSpeech`, `readFile`, `writeFile`, `getDeviceStatus`,
`triggerHaptics`, `openDeepLink`, `listCalendarEvents`, `searchContacts`,
`getMedia`). Prefer platform APIs, CSS and the pinned packages; prefer an app
collection over a remote database unless sharing/sync is an explicit
requirement. Use an external service only when the user asked for it, record it
in the manifest before source relies on it, and never switch silently.

### Image assets (conditional)

Only when the brief needs an original bitmap (photo, illustration, texture, hero,
background), detect whether an ImageGen skill/tool is available. If available and
configured, generate into `public/` and record prompt, source and use. If not,
ask ONCE whether to guide setup or skip; on skip use CSS, gradients, user assets
or an honest placeholder and do not ask again in this task. Use inline SVG or CSS
for ordinary icons.

### Dependencies

Dependencies are locked. Do not add, update or remove packages to implement the
plan; if an idea would need one, redesign it as a source-only implementation. The
only sanctioned dependency path is `LocalAppConfirmDependencyChange` followed by
`LocalAppUpdateDependencies` with the returned one-shot receipt, and only for a
non-core npm-registry package the user explicitly asked for. React, Ionic, Vite,
renderer engines and other catalog core packages are refused there; they change
only through a same-family runtime-profile migration, which is a separate path.
Any add or update raises the native dependency-review confirmation and fails with
`user denied dependency changes` if declined; a pure remove needs no prompt. The
receipt is consumed on use and goes stale if the baseline moves, so a
`dependencies_dirty` error means reconfirm and call again — never route around
it, and never edit package/lock or run npm, npx, Yarn or pnpm directly.

## 4. Build

When the source is ready, call `LocalAppBuild`. It runs the offline production
Vite build from the workspace-backed writable mount, using the fixed runtime and
the app's materialized dependency snapshot, and fails closed on fresh blocking
LSP diagnostics in App-managed JS before Vite starts — so repair LSP errors
first. Keep the necessary compilation and dependency-integrity checks; they are
not optional.

**A successful `LocalAppBuild` is the completion condition.** There is no
automatic verification stage after it, no QA scoring, and no verification-driven
repair loop. When the build fails:

1. Read the failure with `LocalAppLogs {"app_id":"<id>","log":"build"}`.
2. A `not yet available` build means dependencies are not ready: call
   `LocalAppInstallDeps {"app_id":"<id>","wait":true}` and read its `lastError`.
3. If the cause is your source, fix it and build again.
4. If the cause is the HOST — a missing toolchain, a failed dependency install,
   an unavailable runtime — report it to the user and stop. Those cannot be
   worked around from inside the workspace, and retrying will not clear them.

Repair a build failure as many times as the failure is genuinely a source defect
you can identify; do not loop blindly and do not treat a build failure as a
quality-verification finding.

## 5. Deliver

On a successful build, start the preview and hand the app to the user:

- `LocalAppRuntime {"app_id":"<id>","action":"start"}` — serve the built output
  and return the preview url. Restart it if it was already running.

If the build succeeded but the preview FAILED to launch, say so separately and
plainly: the app IS built, the preview did not start, and the launch is
retryable. Never report a successful build as a failed build, and never describe
a preview-launch failure as a quality or verification failure.

Then tell the user, in their language:

- the app entry point (the name they will see and the preview url);
- a SHORT trial checklist — the confirmed `acceptance_checks`, rendered as steps
  the user can perform themselves;
- the status line exactly: `已构建，待你试用`.

Do not run UI operations, take acceptance screenshots, or score the app on your
own. The user tries it. If they report a problem, treat it as a change request:
plan the fix (step 1), prepare, implement, rebuild, and deliver again.

## On-demand testing

Automatic verification is gone from create and modify, but the independent
testing capability is not: when the USER actively asks to test the app, use it.
`$local-app-test` / `$frontend-qa` and the `lingxi-local-app:local-app-use-test`
workflow drive the running preview through the native inspect/act/log tools and
an on-device use test, and `LocalAppCaptureUi` gives a still image when the DOM
cannot describe what is on screen (a canvas or WebGL surface has no inspectable
elements). Run it on request, report what it found, and do not make delivery
depend on it.

## Workspace and dependency boundary

Edit generated source only under `app/`, `src/`, `components/`, `lib/`,
`styles/` and `public/`. `package.json`, lockfiles, `node_modules`, `index.html`,
`vite.config.*`, host metadata under `.lingxi/`, `lib/device-context.js`,
`lib/lingxi-bridge.js` and `lib/platform-adapter.js` are host-controlled for this
workspace. Do not run `npm`, `npx`, `node`, package install/uninstall/reconcile
commands, or alternate scaffold tools — the workspace permission lease is a
filesystem boundary only and deliberately does not lease-authorize package
managers, interpreters or network commands, routing them to the ordinary Shell
approval path instead. A plain approval prompt appearing for `npm install` is not
permission; this ban is yours to keep. Never set `build.outDir` yourself either:
the host passes `--outDir` on the command line and pins `--config
vite.config.mjs`. `src/main.*` may be minimally adapted to import the checked-in
bridge/deviceContext/platform adapter.

Source versioning uses ordinary workspace Git history and the existing Git or
checkpoint capability. Do not introduce a second version store. Create a
checkpoint only after the user approves the working preview, and restore one only
after their explicit choice.

## Existing app operations

Use `LocalAppBuild`, `LocalAppRuntime`, `LocalAppLogs`, structured
`LocalAppInspectUi` / `LocalAppActOnUi`, `LocalAppQueryData` /
`LocalAppMutateData`, `LocalAppCheckpointRestore`, and app-event tools as needed.

For direct `LocalAppMutateData` calls, each operation is exactly
`{"kind":"upsert","recordId":"...","document":{...},"expectedRevision":1}` or
`{"kind":"delete","recordId":"...","expectedRevision":1}`; omit
`expectedRevision` when optimistic concurrency is not needed. Never use `action`,
`create`, `record`, or top-level field values as substitutes.

When the app is ALREADY built and the user asks for a change, do not create
another shell and do not repeat the opening interview: treat it as a MODIFY —
plan the change, approve (the plan omits `template_id`), `LocalAppPrepare` stages
the new contract, implement, `LocalAppBuild`, deliver again. A failed modify
keeps the last successful build and its valid contract.
