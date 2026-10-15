# Claude Code 2.1.289 Desktop Client UI: current gap

This note records the current implementation boundary for Native Mods `Client`
surfaces. It is a gap record, not a Desktop Client parity claim. Source ranges
below were read from `/tmp/harness-claude-code-2.1.289-darwin-arm64`, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.

## Native contract pinned in 2.1.289

The plugin's `<Client>` element carries `module`, `key`, `props`, and optional
`width`, `height`, and `flexGrow`; it has no `children`. `ui.resolve` stamps the
owning plugin identity and prevents a render hook from swapping the Client's
plugin, key, or module. The `module` is a plugin-root-relative surface module,
not a remote URL. Native discovers these modules from the plugin source graph,
resolves imports within the plugin root, and loads a named export from the
module source in a Node VM. That export has the shape `(props, surface) =>
tree`; the renderer's host never imports plugin code into its React context.

The VM surface contract has `state`, `columns`, `rows`, `setState`, `every`,
`onPointer`, `onKey`, and `post`. Its returned surface is driven through
`render`, `setProps`, `resize`, `pointer`, `key`, `runHeld`, `dropHeld`, and
`unmount`. The VM host separately supplies `schedule`, `post`, `startTimer`,
`stopTimer`, and `fault`. `post(data)` serializes plain JSON into a separate
`ui.message` envelope pinned to the parent render component/request id and the
Client key/plugin/module; it is not a render-tree node or a substitute for
Client `props`.

Load, render, and held callback failures are distinct `ui_client_fault`
phases. The fault request is admitted only for the exact currently drawn
`{component, parent requestId, plugin, module, key}` identity. It produces a
plugin-scoped `ui.fault` event whose `requestId` remains the parent render id.
After the hook chain settles, Native checks for a matching listener and checks
that the same Client remains drawn before invalidating its parent render site.
An unmount during dispatch does not cancel the already-started hook, but it
does prevent invalidation. A duplicate is suppressed only when both recorded
`bornAt` and current site versions match.

The pinned ranges and hashes are:

| Native behavior | Byte range | SHA-256 |
| --- | --- | --- |
| Client JSX constructor | 182690001..182691156 | `2186ea7d75b18bf2f3bee33cf85526c4488ddeb2c0017c2b9d7fef3fffa2fc75` |
| Client element schema | 186027762..186028588 | `5f36aab9810e3e5f33ad764891b2029541b3d99b10c8fda29ea6dacb26102d19` |
| `ui.resolve` Client identity stamping | 182871426..182871916 | `7f7b2c725f18b0c6365e387a606b7b89de008fb1094f7332442c263e9db995e0` |
| Surface-module discovery/path resolution | 188332817..188334058 | `b00b52f6aac7c2d493d1ef781287302cc7a593b22f7af307b158dcdfe1f7c8a0` |
| Plugin import-root guard | 182564798..182565594 | `da25454fd89ccc833eecf1e8e59c48b5a18f71fff2afc355d4c32bff049ab59e` |
| VM surface runtime API | 186080973..186093792 | `3f95879a660358dd7bccfd7ac42d1aedf39b2dab00f413f271accfe7bad4eb92` |
| VM module loader | 197823671..197827134 | `d861064afd254ecc64170b9e605048f9e746c515a78ccee9a2861bb91c2147da` |
| Client lifecycle and host bridge | 197828478..197835868 | `3f6f220aa7176322cf61eff70e8db40567f62ddec80d0e9abb07cbc8ea96e8ad` |
| Fault site identity helpers | 197821302..197822152 | `3e2773ea606c93e98db803862026629b3635417546bc0325c2e31b02405f7dac` |
| Drawn-Client registry | 186075934..186077175 | `7b9385ffb1f7026da547ae4ef1f10e863e486009390d0dc12a691e25c7f7bddf` |
| Fault draw gate and duplicate predicate | 186077175..186077601 | `a3f09f2325ca4b87ff367024b4ccfd899dc975306da6a0fba46397b0fd50ce1c` |
| Fault dispatch and post-dispatch invalidation | 186077601..186078649 | `8ddc67706bf42056bb3c4d9915365e3185328e1983a69af629b2266b92cefa19` |
| Fault producer and hook payload | 186079128..186079534 | `fc8b8c3dfcade48ccf4d0fd5d8b3b0366df6acafe7311abe6cd46d112cced267` |

The caller-provided `pointer` and `key` values are forwarded into the VM
surface. These source ranges do not establish a fixed DOM event shape, so a
Product adapter must preserve the typed event payload it receives rather than
inventing one.

## Current implementation — 2026-10-04

Hooks now have a real Bun source-graph compiler, immutable prepared Hook and
Client modules, a session-owned Client Host, Node VM worker, drawn-site registry,
canonical SDK controls and local VM-operation transport. Client module literals
are normalized by importer before compilation, matching the source-pinned
Native Uas route. The Native oracle supplies AST nodes explicitly; it does not
execute the Native parser. Product has typed bridge/IPC/preload consumers and
an AbovePrompt JSON renderer.

Separate frozen runs passed 682 Hooks tests, 563 Client all-feature tests
(366 library tests) and three filtered Orchestrator tests: 1,248 tests in a
composite, not a full-workspace test command. The actual Host TSX lifecycle
covers natural module paths, render/mount/draw, early post ordering, props,
held/state/timer frames, synchronous and asynchronous failure, reload and
unload. Workspace all-feature/all-target strict Clippy passed at v102 after
equivalent lint cleanup. Product workspace/all-target Rust checks passed at
v103 against 66 locally resolved Harness/SDK packages, with its canonical lock
restored. Repository checks passed at v100 against unchanged selected inputs.

The hash-locked isolated Electron DOM fixture passed at v94. It checks early
frame delivery, current held actions, stale high-sequence rejection, one-leaf
fault isolation, no same-environment resurrection, explicit environment remount,
and seven actual runtime mount/unmount pairs exactly once. Status and sibling
from the same plugin both remount when that plugin's environment epoch changes;
a single Client failure in the same epoch does not unmount its sibling. This
uses an injected Host in an isolated BrowserWindow, not the full signed Product
or an enabled Native session. Bridge tests 96, focused Electron transport tests
four, Client-tree tests nine and both Electron typechecks passed at v97.

Worker/helper tests passed 24 at v96. Controlled Native source pins, the
hooks-types asset differential and 1089 actual Worker RPC/Native href-link pairs
plus absent/empty lists and an opaque ticket positive passed. Rust Host requests
include an apiContextTicket; the Worker now admits it without changing URL
admission, and receive treats the preflight reply as terminal. The Host-level
ticketed preflight passed in the Hooks suite. The failed generic Native Client
manager has no implicit props/resize/parent-revision recovery; its terminal
React boundary has a separate fingerprint reset. The external Desktop
consumer remains unpinned.

The local wrapper carries renderRevision/clientRuntimeEpochs outside canonical
Native responses. Ordinary parent revisions do not mint an environment epoch;
actual environment reloads do, including unchanged source hashes. Worker
asynchronous failures publish a failed snapshot without a success tree or
frameSequence, regardless of a matching ui.fault listener. Pending early posts
drain only after draw commit. All failed preflight, test and DOM attempts remain
in the validation receipt; v93's metadata-order fixture, v95/v101 Clippy warnings
and v98's private-helper impl placement are retained with their successful
repairs. SDK 581 inputs, Product Web4/lock and all HEADs/indexes remain unchanged.

A read-only local Bun 1.3.11 experiment passed 11 pure Client VM tests, but the
full Worker exits at its eager node:module.stripTypeScriptTypes import and the
existing node:test runner also has a compatibility failure. Bun supports the
VM classes exercised here. This is measured work for a future runtime migration;
it is not a completed Bun migration or an Electron runtime replacement.

## Remaining acceptance requirements

- Obtain actual mobile/UniFFI runtime and complete Product session acceptance.
  The frozen Host/Worker/Client tests, all-feature compiler checks and locally
  patched Product consumer build do not establish device or release acceptance.
- Execute matching prepared graphs and control chains in enabled Native Mods;
  controlled Gpn/RPC/source probes do not establish the full Native hook session.
- Implement the full Parent tree and all render sites. Product currently
  mounts AbovePrompt and renders Client surfaces plus structural Box/Text;
  the remaining Parent controls and sites are incomplete.
- Pin external Desktop pointer/key and Input/Select callback mapping.
  Native terminal mappings and the injected Product adapter contract do not
  establish the missing external Desktop consumer.
- Retain exact source identities: Native embeds Bun 1.4.3; the currently
  exercised compiler is Bun 1.3.11. Local hooks-types/runtime helpers are
  functional ports with different source bytes and manifest hashes. The
  extracted Native hooks-types asset is available and source-pinned; it must
  no longer be described as inaccessible.
- Close the escaped-lone-surrogate JSON boundary; Rust `serde_json::Value`
  alone does not preserve every Native JavaScript string.
- Obtain enabled Native and visible Product acceptance with matching inputs.
  Controlled Native VM probes and injected-host DOM fixtures remain scoped
  evidence, not full byte-level alignment.

Ordinary `ui.render` invalid-tree fallback stays separate: it keeps the
engine-owned row and does not emit a Client lifecycle fault. The complete
alignment goal, including Mods, remains active.
