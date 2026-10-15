# Claude Code 2.1.289 prompt snapshot materialization audit

## Pin and result

- Binary: `/tmp/harness-claude-code-2.1.289-darwin-arm64`
- SHA-256: `03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`
- Result: Native snapshots preserve the current query's source `systemPrompt` string array. The dynamic boundary marker is a standalone array item only when Native's `Ose()` gate is enabled and at least one shared block exists. Native does not write a `sharedBoundary` field. Fresh snapshots omit the separately-added query `systemContext`; resume reuses the stored array unchanged and recomputes that context.

This is conditional: the actual loopback run's default gate did not emit a marker. The source-only oracle below forces the positive gate to exercise the marker-bearing path.

## Native byte-level path

The byte ranges below are offsets into the exact pinned binary. `// @bun @bytecode` chunks are minified source units; the chunk SHA is over bytes after the delimiter through the next delimiter.

1. Marker declaration: byte `179790383` defines `kM="__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__"`. The relevant chunk is `[179710362,180146192)`, SHA-256 `896026d1b0047d1cabd877df0fe88cba44eb47ca5ba907deafb8799462e19ac4`.
2. Gate: byte `182075655` defines `Ose(){if(!$m())return!1;if(!ni())return!1;let e=He();return e==="firstParty"||e==="anthropicAws"}`. Thus marker emission is conditional on `$m`, `ni`, and provider. Its chunk `[181688553,182403436)` hashes to `d391ae978838e3edee0b4c8027bcfa6d5af41aa609becb479d78d9de61d9e028`.
3. Default prompt assembly: `WA` at byte `188654241` ends with `UDn(s,Ose()?kM:null)`. `UDn` at `187625913` filters the marker token from section text, then returns all `scope==="shared"` text, the marker as a standalone element when enabled and shared text exists, then all `scope==="session"` text. The `UDn` source is in chunk `[187181803,191691521)`, SHA-256 `41c6b9b4ce4d6849c85c5405544498cda725d79655ae9aa37f5531a51f283b93`.
4. Query entry at byte `209686148` obtains `un` from `WA(...)`, calls `Mj({...,defaultSystemPrompt:un,...})`, stores it as `vo.renderedSystemPrompt`, and passes it as `Wn.systemPrompt`. `Mj/mho` at byte `188728142` chooses the default array and returns `Qs([...])`; `Qs` at byte `185935736` is the identity `function Qs(e){return e}`. The prompt array, including a gate-produced marker element, is not joined or filtered there.
5. Fresh snapshot writer: `il` at byte `195404912` takes its incoming `o` and its live-recorded path calls `ln({type:"prompt_snapshot",systemPrompt:[...o],...})`. Query-loop callsite at byte `195438456` is `qt=il(w,s,{...})`; it yields `qt.record` into `w` before computing `dd=Qs(j0o(qt.systemPrompt,Ye.systemContext))`. `j0o` therefore appends separately-built system context after the snapshot boundary.
6. Attachment persistence: `ln` at byte `190665106` returns `{attachment:e,type:"attachment",...}` and retains `e` as the payload. The query yields `qt.record` before the model call.
7. Resume: schema `LEn` at byte `187331525` declares `systemPrompt:T(o())` and has no `sharedBoundary` member. `N1` at byte `187332636` reads the latest `prompt_snapshot`, parses it with `LEn().safeParse`, and sets `systemPrompt:Qs(s.systemPrompt)`; it does not remove the marker or derive an offset. `Qs` is identity.
8. Cache conversion: `B$r` at byte `186326847` detects `e.findIndex(M=>M===kM)`, excludes `kM` from resulting provider blocks, and uses its index to split shared/global from session/org cache scopes. This corroborates that the marker is persisted prompt structure used by the provider cache projection, not user-visible prompt text.

A byte scan of the entire pinned binary found exactly two occurrences of `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` (string table and declaration) and zero occurrences of `sharedBoundary`.

## Executable evidence

`node scripts/tests/mods_prompt_snapshot_289_oracle.mjs --check` passed. It hash-gates the binary, extracts the exact embedded `Ose`, `UDn`, `Qs`, `Mj/mho`, fresh `il`, `ln`, and `N1` functions, then executes the marker-enabled path. It observed:

```json
{
  "providerComposerOutput": ["native shared base", "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__", "native per-session context"],
  "freshSnapshotAttachment": {
    "type": "prompt_snapshot",
    "systemPrompt": ["native shared base", "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__", "native per-session context"]
  },
  "recoveredPrompt": ["native shared base", "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__", "native per-session context"],
  "sharedBoundaryFieldPresent": false
}
```

Oracle limit: this runs extracted Native functions with stubs for provider/session/environment dependencies, the visible `systemPrompt: string[]` schema contract, time/UUID/hash and telemetry. It is not a full native subprocess run or a test of provider rollout state. The script replaces two unused coordinator-only `import.meta.require` expressions so Node can parse the extracted `Mj/mho` source; `Ta()` is forced false, so that branch is not reached.

The separate actual Native loopback probe supplied by Root is receipt `/var/folders/2c/gch8wj4s0d52rygkyd6b9f_h0000gn/T/mods-native-row-wire-289-UAYC4a/receipt.json`, pinned to the same binary. Its actual JSONL file hash matches receipt SHA-256 `6d59c7d1d82fd3a5a12dc628662c547f4ca2f28ebb1560d3a8ea773559a50ec3`. Actual JSONL prompt-snapshot rows 13 and 17 each have a 10-element string array, no marker, and no `sharedBoundary`. This is actual writer evidence for that run's default gate; its probe acceptance flag was false for a separate request/fixture condition, so it is not general run acceptance.

## Concrete host fidelity gap and correction

Current host code [prompt.rs](/Users/luolingfeng/lingxi/harness-runtime/crates/orchestrator/src/conversation/prompt.rs:624) strips the provider marker into a byte offset, trims the dynamic-context suffix, and stores `vec![static_prompt]` plus `shared_boundary` at lines 624–638. The serializer writes `sharedBoundary` at lines 699–700. The core [PromptSnapshot](/Users/luolingfeng/lingxi/harness-runtime/crates/core/src/host/orchestrator.rs:624) names that serialized field. This differs from Native in both schema and representation: Native preserves the exact `systemPrompt` block array, conditionally including its marker element, whereas Host collapses it to one joined string and invents an offset field. The local test at `prompt_snapshot_tests.rs:58–98` currently asserts the inverse Native behavior.

Current-only correction: capture the host's pre-`j0o` static prompt block vector before joining it; serialize that vector verbatim as `systemPrompt`; never serialize `sharedBoundary`; carry the marker only as its own vector element when the matching Native gate produced it. Keep any split/marker interpretation transient inside provider conversion, where the marker is consumed to apply cache scopes. Resume should restore the saved vector exactly and append newly computed system context after it, matching Native's snapshot-before-`j0o` boundary. Update the test to assert both default no-marker and positive marker-bearing shapes. Do not unconditionalize the marker: Native `Ose()` gates it.

## Window hashes

These are SHA-256 values over the listed exact binary byte windows:

- `[179790300,179790850)`: `4c809575856bf61dd71c543b8e4b1b4fdfc16aa3478334ceb272aba29044b780`
- `[182075400,182076200)`: `541994651e427a02a5c7c717a059dba5f0d71d65abfa98855c356a309e6d5392`
- `[187331525,187332444)`: `b0bf86125a6e8440cf7cff280a7eca96bdd011d9f77e5338557b05bab96a18ba`
- `[187625800,187626500)`: `5c269b0f40e15f03edb39175f9c1afaf3fcd17a20deaa4ae4a63a70db3683d8d`
- `[188654100,188654800)`: `90d41d9e32d408db7dc8b81d691a723bdce0df59fcc072e4ec24dce826149939`
- `[188728050,188728900)`: `0cb3b53304f6fbfdc645eb2a213957ee7fbcd5cf58acd4e4a6eb2e3edcc9bfba`
- `[195404850,195407100)`: `38b817c8a6c36b3844b93572e7a6efd40f31f2668d330d17af9846229a9cfbab`
- `[195438350,195439200)`: `80df4fbddcd32bbc0d6b25d16fbdb2ec4659d55fbdfde5fc001612e90770e3bc`
- `[190665050,190665500)`: `29c0ad7a2b80f70be14f364fcafc8fa8440a03c5dc2f0cccb4c33a2c2bbb0809`
- `[186326847,186329000)`: `9bba7bfcd547db155909fa57e45793baf99ddc5b747670b4c099382ddb8179bd`
- `[209686100,209686700)`: `3f10427152ee166ee1b5956bfb7ad03fddc1f0fdee2c05962ef84f67f198e38b`
