# Claude Code 2.1.289 agent API oracle facts

## Scope and method

The [recorded VM results](mods-agent-2.1.289.json) include the source windows,
status/identity cases and serialized JSON UTF-8 bytes. These are Native source
functions under injected host facts, not installed-Mod runtime output.

Pinned executable: `/tmp/harness-claude-code-2.1.289-darwin-arm64`, SHA-256 `03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.

The companion `/tmp/claude-289-agent-api-oracle.mjs` reads the listed byte spans from this executable, evaluates the exact pure JS helpers in a Node VM, and stubs only host-owned task rows, name/provenance maps, and team-file reads. It does not call a provider, network, or app startup. `node /tmp/claude-289-agent-api-oracle.mjs` passes. Its final lines include exact JSON-string UTF-8 byte lengths and hex for representative values.

The checked-in probe runs with the pinned source and emits the same raw cases:

```sh
node scripts/tests/mods_agent_289_oracle.mjs /tmp/harness-claude-code-2.1.289-darwin-arm64 --check
```

The complementary [lifecycle probe and fixture](mods-agent-lifecycle-2.1.289.json)
execute the exact Native `eU`/`Yyo` launch path and selected lifecycle helpers
with explicit host seams:

```sh
node scripts/tests/mods_agent_lifecycle_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64 --check
```

That controlled probe injects W1's Agent-tool result and a task registry; it
does not run the actual tool catalog, permission engine, team file store, or
application.

## Source anchors

| Source | Byte range | SHA-256 | What it proves |
|---|---:|---|---|
| `$agent` wrapper `rw` | `[182844300,182844999)` | `1f639d650c75229f9e9ebac6cf50ae7694bb4e6fea77fec2763158c8966b47a7` | list/spawn wrapper validation, event arguments, return shaping |
| `h_e` task identity projection | `[188340930,188341386)` | `676c1b52ae40467ef77100eb7bd9601729b8e4a9617ee7c543f9f4ad619a1f44` | local versus in-process stable ID and optional identity fields |
| `PAt` / `MAt` | `[188506311,188507089)` | `d36381136a0e262d37de6838e1d3d623451a01899a02a2d78b43c6f512691a9e` | raw status mapping and inactive team-member override facts |
| `ifo` list reducer | `[188507089,188507928)` | `e003640923c46f03788a530b61ae435e10be9c2ba9dbe9713195b5257d6cba15` | filtering, dedup winner/order, optional output fields, status layering |
| plugin hook context `$At` | `[188508612,188508693)` | `728dd2ce55ef1c33361b9377a85efb109048e09ca338f7258e2c689e0db4e4ee` | `hookCaller` is the current plugin ID; `hookOrigin` is separate |
| `GTe` activity predicate | `[188505877,188506078)` | `b0cc613b8c0411e79e0d9470f30f95ed73402e90a6dfb1b477d0a75f55526120` | running background Bash and monitor promotion |
| keepalive idle-reason constant | `[188506130,188506156)` | `56d4037ee4da4a91e82e03ab2289eb92f335720636fa323a924f19fa71bc3f38` | `flag:idle-window` special case |
| `VOt` | `[188857631,188857723)` | `7a7eaa54af7e2de2de9d4fb2fc464335ff9c058bf91fc8c9761c14b360a872f7` | host result enrichment with `agentId` and `resolvedModel` |
| `c_o` teammate bridge | `[188857756,188858131)` | `40d56052b307904e32d76b790f1c7a9bd8434863cf3fda9b7cb00700cad99ced` | teammate task-row selection and raw-ID fallback |
| `CMr` provenance writer | `[188963827,188963992)` | `b4a9186fc470ab1b5ad28b53ca6d5d8856ec7e8506c36d2bd8f562f18e339259` | stores/deletes `{hookOrigin, hookCaller}` by stable agent ID |
| `Py` team-file reader | `[186836941,186837441)` | `de6f3328d7a7cbbcb620d50e432ef40c2af070bd6baf2f5fbe78814248f61ce6` | V5/filesystem read and null-on-missing/error behavior |
| `bn` agent spawn hook dispatcher | `[194820648,194821115)` | `34306bd3e01fba34e855d3afee63788a83fd860c71c9f383cbca4beb3157f485` | common `agent.spawn` hook gate and rewrite/deny path |
| named teammate caller | `[194832917,194834998)` | `5996c73dc5590ab0a8c2579029c04b31680eb434e399c45a4df621f5675b852e` | named teammate calls `bn(..., isTeammate:true)` before teammate launch |
| embedded missing-name-registry error text | `[78657088,78657164)` | `059760913df7073fb0e0d12b842fa838978bbbefb9dc0dc0ab58142cdd0cf88a` | binary resource, not a Node VM throw string |
| embedded missing-provenance error text | `[78657172,78657246)` | `edda4f2c11efec10fe504625527248ef8eb90bae2d2154800f7fa49dae0e9207` | binary resource, not a Node VM throw string |

`ifo` internally calls `Rs`, whose terminal set is exactly `completed`, `failed`, and `killed`; other statuses, including `paused`, are nonterminal for duplicate precedence.

## Native lifecycle producer anchors

The second probe records the full source windows and executes the following
bounded pieces of Native code. The absolute offsets are within the pinned
binary named above.

| Native producer/helper | Byte range | SHA-256 | Finding |
|---|---:|---|---|
| `eU` launch-only Agent result | `[188853212,188855653)` | `a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66` | Runs `W1` with a virtual Agent tool-use; on `async_launched`, starts detached settlement and returns `{result:{agentId,resolvedModel},text:""}` immediately. |
| `Yyo` per-plugin quota lease | `[188855653,188856054)` | `871a5bd9cefdb815fd357ef4ada0687c23924b4e2c766ec4b8f166806e1ccf87` | Default cap is 20 (`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` overrides); cap is checked before executor; lease is released at executor completion unless `leftRunning` transferred it to the settlement promise. |
| `BOt` / `UOt` detached settlement | `[188853103,188853212)` / `[188856054,188857230)` | `caa26e4acdc771ea59d5ecfdc7ab29d7d447a55ce62bfaa6537b302aa5a7b0ad` / `5cc59e9e5cb646dae8f99b544e87f369842ae2b5db725146ceb7b076374573bb` | Uses a fresh abort signal and infinite settle budget, polls registry every 150 ms, and releases the quota lease when the task reaches a terminal state or disappears. |
| direct `agent.spawn` Host endpoint | `[188896338,188896451)` | `f6a3fad95285deacb10c27fe5a386824309967a48edeef3e3ae89832b90e5310` | Delegates to `Yyo(AgentTool executor)` and then applies `c_o` teammate-result enrichment. |
| local row constructor `bJ` | `[191176717,191177565)` | `c5ad60d5cc38f5a90ab6d79f4be405b40c54575d2b5b21e842825b50831cff9e` | Creates a local row with `isIdle:false` and an empty keepalive-reason Set; it does not initialize `finalizing`. |
| local query runner `N5` and row branch | `[194760296,194770594)` / `[194764875,194765302)` | `c3fbc7491ba06e1c1b4297004f310679fcf8df1eecb622159bcff42160783021` / `b313f4db1c32c2968e3c7f77813a8a9cffda4e943fd3ec5c157771638f6305ef` | Adds every ID from a complete assistant row before computing idle; tracks only canonical `Agent` tool IDs separately; removes IDs as result rows arrive. The local row is idle only while at least one pending tool-use exists and all pending IDs are Agent calls. |
| `EMr` and `kHo` finalizing transition | `[191175230,191176232)` / `[191176232,191176367)` | `9c70d273542cf503d4e25e12bec5ea203a086ad33161c575f29045a51666ecec` / `5e3643b9197b3fa4a06280502c660b0650f1fa461729eb630222b73c219fa4a1` | Writes completed status plus finalizing before completion notification, then clears finalizing in the completion `finally`. `N5` initializes its `finalizingPass` local to undefined and does not assign it in the pinned window. |
| keepalive Set helpers `YP` / `hx` | `[185181785,185182220)` | `5afaa202299fcb45a856631a642f21b9c48dc42c44eac2d579969e15afe76413` | Adds/removes one reason idempotently; removing the last reason from a terminal, non-retained row sets delayed eviction. |
| `Yq` owned-child cleanup | `[191166682,191167081)` | `f642cb79e37b7f633ee77002f33fe1696bb646e212d72d969e15d20642f2540c` | Removes `agent:<id>` and other owner reasons when the child is missing, notified, or no longer owned by the parent; it does not equate tool-result arrival with child settlement. |
| ordered `agentNameRegistry` writer `pwo` | `[200985406,200986838)` | `99d75adb94adcb3d2fc8e7052c0b1c547a8c30e70d714be56cedb85f3eeda7e8` | Uses an insertion-ordered Map; same-key overwrite retains position; name lookup for duplicate IDs returns the first inserted matching key. |
| `VOt` result model projection / `c_o` teammate bridge | `[188857631,188858131)` | `f6999ad4718a1d17944ae61c5f02fc8d1f70eb16fe3b55f376fe7c4edb0c2e97` | For a successful teammate spawn, preserves the raw result and adds `resolvedModel: At(model)` plus the selected stable `agentId`. |
| `At(model)` canonicalizer | `[182058341,182058885)` | `2c89875db104852dc30d6bdcc8746bd27ba8fe89098abd42d4dea745d44a50ae` | Trims input, handles aliases and `[1m]`, applies first-party legacy Opus remaps when enabled, then maps through the model identity helper. |
| Native model alias/default helpers | `[179588224,179588545)`, `[182034000,182035355)`, `[182029166,182029364)` | `20d559fcbc34c301f5f2ddd6a6a09e5ee9affa43e71625890d9e73fe42546340`, `6504246fb40bcc2967d1ec59b47e6c3dc7fdbbbe918b31fbc073018ec276bb9b`, `e0a5ee06a2e39bebd4631b2ae437a633f51fab55d640db6ca408e0a37a522cfe` | Source alias set includes `sonnet`, `opus`, `haiku`, `fable`, `best`, suffixed 1M aliases, and `opusplan`; actual defaults come from Native config/provider helpers and `best` consults the live best-model selector. |
| Embedded Fable alias catalog entry | `[180324304,180324378)` | `9b81701cb225f228ae81148601d52b486008de8d820e3b9b450662f73b98b007` | Actual baked catalog says `fable.default = "claude-fable-5-1"` and `fable.per_provider.gateway = "claude-fable-5"`. |
| Embedded Fable 5 / 5.1 model rows | `[180320565,180321532)`, `[180321533,180322597)` | `128508c0ed612a156f58caf41ed94ad8ebbf4f9863300e0c60d659ce40080428`, `1aa57accbbe4542ee0cdbeb268a309968f7804ac82fa7a1fac7f98fb853e8789` | The baked rows identify provider IDs for each alias target; Fable 5.1 uses `us.anthropic.claude-fable-5-1` on Bedrock and `anthropic.claude-fable-5-1` on Mantle, while its first-party ID is `claude-fable-5-1`. |
| Catalog alias provider selector `QWe` | `[180328974,180329131)` | `776928835e74d1e4a4324635b00e6f0d32cbd50f41b2899fcf41c887c58df8df` | Selects `per_provider[provider]` when present, otherwise the catalog alias `default`. |
| Catalog provider-ID mapper `D` | `[180329878,180330236)` | `c83cd89310ff7e65b796e506d3ac699e29981ac568623fde8364585d843c6f83` | Maps the catalog's `provider_ids` into the configured provider model record, including the Gateway ID. |
| Fable override normalizer `nO` | `[182061267,182061314)` | `1d5c5e72e07948c66b0b99c9fbf309f0565af2c66749065b5bc71e865167c4b6` | Removes `[1m]` occurrences case-insensitively. Since `k0e` tests the Fable environment value against `undefined`, an explicitly empty override stays empty through `qb`/`lA`; only a missing override goes through `Wd` and runtime provider/catalog defaults. |
| legacy Opus list and remap flag | `[182059234,182059432)` | `8dacfaf870f77868a191a159fbf64080d1101b8e4eff032b6d645a341229b904` | Only the four source-listed Opus legacy IDs remap, only when `ad()` recognizes first-party and `CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP` is false. |
| Native `[1m]` flag/gate and suffix builder | `[182063933,182063987)`, `[182063987,182064540)`, `[182037196,182037253)` | `e3564c21fea9fc724c50317934f506fabcd8a61df2aec800660b2f2371f1bab8`, `4d474daa66af006303903b33d831d4bb84d0f34baa7da85a619102cfbbc4c057`, `6a671212aca2a2b68db48030c2be9c02fc125ccdc98c56e3dce359d3eff17dd0` | `[1m]` is controlled by the explicit disable flag and context support; `[2m]` is not handled by this `At` path. |
| team member active writer `oBt` | `[186842404,186842863)` | `08e29f637c4d9b81cebb846b5ed16197147ff0b8fbeb66b3c941e9c1cc5f5bd0` | Finds the first exact-name member, updates only `isActive`, skips missing/no-op writes, and delegates persistence to the lock-backed file updater; it catches and logs write errors instead of propagating them. |
| `Py` team-file reader and `Vhn` path sanitizer | `[186836941,186837441)` / `[186835822,186836941)` | `de6f3328d7a7cbbcb620d50e432ef40c2af070bd6baf2f5fbe78814248f61ce6` / `c1e196c4426d3e9a362133ab862011fc5bcd267f2b9c63874eec17d39f2282c1` | Sanitizes to lowercase ASCII alphanumeric with each other UTF-16 code unit replaced by `-`; reads `v5:teams/<slug>` when V5 is supplied, otherwise `<configDir>/teams/<slug>/config.json`. |

The companion VM confirms the spawn quota result and release boundary under a
controlled cap of one: a second same-plugin launch is refused while the first
launch's registry row remains running; after that row completes, a new launch
is accepted. The production default remains source-pinned at 20. The VM stubs
the 150 ms wait as 1 ms to keep the probe fast.

The true team-member write occurs at current teammate query entry; the false
write occurs from a nonleader teammate `Stop` hook and a teammate failure
notification path. These are task/member facts, distinct from local-Agent
status. The probe executes `oBt` with a lock-backed in-memory file seam and
proves same-value writes are skipped. It does not inspect or mutate a real
team config.

`N5` runs `Yq(parent, registry)` after the parked/rest path and again during
ordinary successful completion, before the parent result projection. It walks
the parent's current keepalive reasons, handles only `agent:`, `workflow:`,
and `bash:` prefixes, and skips a reason while `ez(parent, child)` is true. It
removes an absent child; a child marked notified or owned by a different
parent; a notified workflow; and a Bash row only after it is both notified and
terminal. Thus arrival of a child tool result does not itself release the
parent's `agent:<id>` reason. Other Native reason prefixes are maintained by
their owning subsystems and are not removed by this helper.

## `agent.list`: identity, ordering, and duplicates

`rw.list` makes exactly `t("agent.list", {})`, ignores caller arguments, and returns the host value without reshaping it. The host reducer visits `Object.values(taskRegistry.all())`; it does not sort rows.

`h_e` projects rows as follows:

- `local_agent`: stable `id = agentId`; `teammateId` and `name` are undefined; `description = description`; `type = agentType`; `parentId = parentAgentId`.
- `in_process_teammate`: stable `id = resumableAgentId ?? identity.agentId`; `teammateId = identity.agentId`; `description = spawnedWith.description ?? row.description`; `type = spawnedWith.agentType || "teammate"`; `parentId = spawnedWith.parentAgentId`; `name = identity.agentName`.
- Other task kinds are omitted from the returned list. They remain available to the activity predicate described below.

The reducer stores results in a `Map` keyed by projected stable ID. Its winner rule is source-order-sensitive:

1. A row is terminal only for the exact statuses listed above.
2. Every nonterminal row marks the ID as having a nonterminal and is applied; therefore the last nonterminal duplicate wins.
3. A terminal row is skipped once any nonterminal duplicate has appeared. A prior terminal is overwritten by the first later nonterminal; later terminal rows cannot replace it.
4. If an ID has only terminal rows, each applies and the last terminal wins.
5. Replacing a `Map` value does not change key insertion order. Final array order is the first occurrence of each stable ID in the source rows, not the winner row's position and not a lexical sort.

Observed serialized list sample (exact `JSON.stringify`, 322 UTF-8 bytes):

```json
[{"id":"agent-A","description":"A last active","type":"Explore","status":"running","parentId":"parent-A","spawnedBy":"spawn-plugin"},{"id":"agent-B","description":"B first-seen","type":"Explore","status":"running","name":"B mapped name"},{"id":"agent-D","description":"D last terminal","type":"Explore","status":"failed"}]
```

A second fixture put a terminal row before a paused row and another terminal row. The paused nonterminal won and serialized as `[{"id":"agent-C","description":"paused nonterminal","type":"Explore","status":"idle"}]` without whitespace (actual bytes are printed by the script).

## List status truth table

`PAt(row, hasName)` runs first. `MAt` may then replace a teammate status with `idle`. Finally `GTe` promotes only an `idle` result to `waiting`.

| Row/status facts | `PAt` result | Evidence/priority |
|---|---|---|
| local running, `isIdle !== true` | `running` | baseline |
| local running, `isIdle === true` | `waiting` | local `isIdle` is the first strict-true predicate |
| teammate running, `awaitingPlanApproval === true` | `waiting` | wins over teammate `isIdle` |
| teammate running, no approval wait, `isIdle === true` | `idle` | teammate idle state differs from local idle state |
| local completed, `finalizing === true` | `running` | wins over keepalive reasons |
| local completed, any keepalive reason other than `flag:idle-window` | `waiting` | unusual keepalive wins over name/idle rule |
| local completed, only `flag:idle-window` | `idle` | because a keepalive reason exists |
| local completed, no reasons, display name exists | `idle` | name availability is enough |
| local completed, no reasons, no display name | `completed` | no idle evidence |
| teammate completed, with `finalizing`/keepalive fields | `completed` | teammate path ignores those local-agent fields |
| any `paused` row | `idle` | `PAt` explicit mapping; still nonterminal for dedup |
| other status (e.g. `failed`, unknown string) | same status | passthrough |

`MAt` only examines rows with all of: kind `in_process_teammate`, status `running`, and `identity.resumableAgentId === undefined` (strict undefined; `null` does not qualify). It reads the team file and collects member IDs whose `isActive === false` (strict false). It then sets those matching teammates to `idle`, even if `PAt` had returned `waiting` for plan approval. `isActive:true`, missing membership, absent team files, and failed reads do not add an inactive override. A present resumable ID skips this membership read even though nullish identity projection would fall back to the raw teammate ID if the resumable ID were null.

`GTe` examines all task rows and matches exact `agentId === projected stable ID` plus `status === "running"`. It returns true for `local_bash` only when `Boolean(isBackgrounded)` is true, or for `monitor_mcp` / `monitor_ws` regardless of `isBackgrounded`. `ifo` applies it only when the already-computed status is `idle`; it changes that to `waiting`. It does not change `running` or an existing `waiting` status. VM cases confirmed inactive + background Bash => `waiting`, inactive + foreground Bash => `idle`, inactive + monitor => `waiting`, resumable teammate with an inactive team-file entry => unchanged `running`, and monitor attached to an already-running teammate => unchanged `running`.

## Optional output fields and provenance

The output is assembled in this order: `id`, optional `teammateId`, `description`, `type`, `status`, optional `parentId`, optional `spawnedBy`, optional `name`.

- Optional spreads check `!== undefined`; explicit `null` is serialized as JSON `null` for `teammateId`, `parentId`, and `spawnedBy`.
- A missing teammate address omits `teammateId`; a null address keeps `teammateId:null` in list output. The spawn wrapper is narrower: it returns `teammateId` only when `teammate_id` is a string, so null is omitted there. Empty string is a string and is preserved.
- `parentId:null` is preserved; missing parent is omitted.
- For name, `h_e.name ?? nameRegistryName` is used; an empty string is present and emitted, null falls back to the registry, and no name omits the field.
- `$At` sets `hookCaller = r.plugin` and `hookOrigin = r.origin`. `CMr` stores the scalar caller plus origin under the stable ID; if caller is undefined it deletes the entry. `ifo` exposes only `hookCaller` as scalar `spawnedBy`; it does not return `hookOrigin`. Native caller values come from plugin identity. A null caller was separately tested in the VM only to prove the `!== undefined` JSON presence behavior.
- The emitted `type` is the agent type (for example `Explore`), not the task-row kind (`local_agent` / `in_process_teammate`).

Null/missing fixture exact serialized JSON (254 UTF-8 bytes):

```json
[{"id":"stable-null-address","teammateId":null,"description":"row fallback description","type":"Explore","status":"running","parentId":null,"spawnedBy":null,"name":"Teammate"},{"id":"stable-missing-address","type":"Explore","status":"running","name":""}]
```

## `agent.spawn` wrapper and teammate bridge

The wrapper sends `t("agent.spawn", mf(input, prompt))`. `mf` emits `tool:"Agent"`, the prompt, the caller description or the first five prompt words, `run_in_background:true`, and only supplied `model`, `subagentType` (as `subagent_type`), `name`, and `cwd` fields. A missing/non-string/whitespace-only prompt throws exactly:

```text
oracle-plugin: $.agent.spawn takes { prompt, ... } (a non-empty prompt)
```

The wrapper reads a string `resolvedModel`, `agentId`, and `teammate_id`; return `model` is `resolvedModel ?? input.model ?? "inherit"`. It returns `agentId` only if string, and maps string `teammate_id` to `teammateId`. If host response has `deny`, or `isError === true` (using `text` as the denial), result is `{deny: ...}`. VM cases produced:

```json
{"model":"resolved-model","agentId":"loop-id","teammateId":"worker@team"}
{"model":"inherit"}
{"model":"inherit","agentId":"stable","teammateId":""}
{"deny":"denied"}
```

First success result is exactly 73 UTF-8 JSON bytes when stringified. `agent.list({ignored:true})` still dispatches `agent.list` with `{}` and returns the host array directly.

For `status:"teammate_spawned"` with string `teammate_id` and no denial, `c_o` filters task rows by `h_e(row)?.teammateId === teammate_id`; it chooses the first nonterminal matching row, or the last matching row if all are terminal, and takes that row's projected stable ID. If none matches, it uses the raw teammate ID. `VOt` preserves the result, adds `agentId`, and adds `resolvedModel = At(model)` only when `model` is a string. Null/non-string `teammate_id`, other statuses, or denied results do not get this enrichment. VM cases confirmed first-live, last-terminal, raw-address fallback, and null-ID no-op.

`At(model)` is a model canonicalizer, not a transparent copy. The exact source
trims the value; handles case-insensitive `sonnet`, `opus`, `haiku`, `fable`,
`best`, and `opusplan`; applies `[1m]` handling subject to the disable flag and
model-context support; and remaps only the source-listed legacy Opus IDs when
the provider is first-party and legacy remapping is enabled. `[2m]` is not
handled by this path. The lifecycle fixture executes the exact Native helper
with controlled default-model values, provider identity, a best-model
catalog entry, and 1M-support facts. It confirms that non-alias input retains
its casing after trimming, the defaults/aliases choose the corresponding
helper, `best[1m]` drops the suffix, and legacy remapping is first-party and
flag gated. Those controlled outputs do not assert the current machine's
actual provider defaults or best-model catalog. Harness's
`resolve_user_specified_model` is an alias-resolution subset and has not been
proven equivalent to every Native `At` dependency; the resolver owner is
aligning those branches separately.

The Fable default is now pinned against the actual embedded Native catalog,
not inferred from the controlled `ANTHROPIC_DEFAULT_FABLE_MODEL` used by the
`At(model)` vectors. With that environment variable missing, `k0e()` calls
`Wd()`, which selects `pa("fable", currentModelMap) ?? currentModelMap.fable51`.
`pa`/`Yb` use the current provider to resolve the catalog alias through
`QWe`: the catalog's first-party/default alias is `claude-fable-5-1`, while
Gateway explicitly selects `claude-fable-5`. The exact `QWe` helper was run
against the Fable alias literal extracted from this binary for all provider
keys, and the model rows' provider IDs were read from their exact source
records. Thus `claude-fable-5-1` is verified for the first-party default;
there is no provider-independent Fable wire ID. The current Host default
matches that first-party ID, while the Gateway alias and provider-specific
wire mapping remain a parity gap. The probe does not load this machine's
runtime provider cache or `modelOverrides`, so it does not claim a complete
effective runtime default for every configured account.

Named teammate launch also calls the same `bn` hook dispatcher with `isTeammate:true`, `parentAgentId` from the parent, model/description/name/permission facts, and the incoming hook origin. `bn` checks `Cc("agent.spawn")`; when enabled it invokes the plugin hook pipeline and applies its returned agent/spawn rewrite or denial before `spawnTeammate`. A denial becomes `Subagent spawn denied by a plugin: ...`. If the hook gate is disabled, `bn` returns the original spawn facts and default starting guard. So the concrete answer is: named teammates do go through the `agent.spawn` hook path when that event is enabled; they are not a separate hook-free spawn path.

## Current Harness task facts and gaps

Rechecked on 2026-10-04: `TaskRows::ordered_states` preserves insertion order,
and `TaskRegistryHandle::list` assigns each row an explicit `source_order`.
Replacing a row retains its position; removing and reinserting moves it to the
end. `reduce_agent_list` implements the Native duplicate/status projection.
The current tree now has an Orchestrator composition caller, but that Rust
route is still pending the Root-owned Cargo/test freeze; it is not covered by
the Native JavaScript VM probe.

`TaskRecord` retains the ordinary TaskList contract. Its host-only
`AgentTaskFacts` sidecar now carries missing/null/value presence and some real
source facts: local stable ID/type, spawn caller/origin/description, parent ID,
activity ownership and background state; teammate stable/address/name/type,
spawn provenance, idle and plan state; shell and monitor activity ownership.
These facts are not inserted into the ordinary TaskList JSON. Sources are
`crates/core/src/host/task_registry.rs`, `crates/tasks/src/lifecycle_store.rs`
and `crates/tasks/src/handle.rs::agent_task_facts`.

The following boundaries remain incomplete and must not be inferred from
nearby fields:

- The Worker and Hooks direct API routes now compose through
  `ConversationOrchestrator::mod_agent_list` and `mod_agent_spawn`. List uses
  ordered task rows, the insertion-ordered name registry, and qualifying team
  membership files. Spawn prepares the canonical `Agent` tool, dispatches
  through the tracked tool path, then retains its per-plugin lease for detached
  `async_launched` settlement. These current-tree paths are implemented but
  still pending the Root-owned Cargo/test freeze; the Native VM probe does not
  execute their Rust composition.
- The Harness `AgentNameRegistry` now enumerates in insertion order and is
  wired into the conversation builder, and the list reducer now consumes it
  with the ordered task snapshot. The static agent-definition catalog remains
  a different dataset.
- Local `isIdle` is now updated from complete assistant tool-use rows and each
  tool-result row by the Rust runner's `LocalAgentToolUseTracker`, mirroring
  Native `N5`; it is no longer an absent fact. The local completion and
  persistent-rest paths now atomically publish completed+`finalizing:true`,
  keep that fact while the terminal/rest notification is assembled, and clear
  it after the notification drain marks the row notified. A focused registry
  regression covers the visible interval; these current Rust changes remain
  pending the Root-owned Cargo gate.
- `keepaliveReasons` is no longer wholly absent: the sidecar starts with an
  empty set-like vector, and background local-Agent child admission writes
  `agent:<uuid>` to the parent, with removal on launch failure and stale-child
  reconciliation. The full Native collection and its other reason sources
  (idle window, Bash, workflow, monitor, kill-unsettled) are not all wired.
  The Rust idle tracker follows assistant/tool-result boundaries; Native also
  emits a distinct `fallback_sweep` reason transition, which remains an
  explicit producer gap. `is_parked`, terminal status, and delayed eviction
  remain distinct facts.
- Team activity now has a current producer path: `TeamMemberActivityHandle`
  is installed by Desktop assembly, `TeamFileMemberActivity` updates the
  coordinator roster under its file transaction lock, and the in-process
  teammate handler writes strict true/false transitions. The Host mapping sets
  active after successful pool allocation and after successful resumed
  `UserMessage` injection; it clears on allocation failure, worker failure,
  and stop. Those are explicit Host lifecycle mappings, not proof of the
  complete Native teammate runner.
- The list composition reads local team config for a running teammate whose
  `resumableAgentId` fact is Missing and uses strict `isActive === false`.
  Native also has a V5 team-store read path, which this composition does not
  yet support. The current teammate producer normally fills resumable ID from
  the stable ID, so ordinary production rows do not satisfy the Native read
  gate. Native's external teammate identity producer omits `resumableAgentId`,
  but the current Host sidecar does not preserve that independent missing fact.
  Root's Rust integration tests remain pending.
- Direct spawn currently maps selected `model` through
  `resolve_user_specified_model`; Native `VOt` uses the fuller `At(model)` path
  described above. Policy is aligning the alias, legacy-remap, provider, and
  context-suffix branches. The Native VM's `async_launched` result remains a
  controlled W1/registry probe, not a live Rust spawn execution.

The existing `SubagentListingEntry` / `agent_listing_entries` are static agent-type catalog rows (`agent_type`, guidance, and tools description), not a running-agent roster. They cannot stand in for `$.agent.list`.

## Error evidence and limits

Invalid spawn prompt uses an explicit source-built error; Node VM caught that exact message for both missing prompt and whitespace-only prompt.

For deliberately missing list context, the Node VM throws:

```text
Cannot destructure property 'agentNameRegistry' of 'DQn(...).namedAgents(...)' as it is null.
Cannot destructure property 'spawnProvenance' of 'dt(...)' as it is null.
```

The executable also embeds resource strings with different wording:

```text
Cannot destructure property 'agentNameRegistry' from null or undefined value
Cannot destructure property 'spawnProvenance' from null or undefined value
```

Those strings are not the Node VM's thrown messages. Since this audit evaluates extracted helpers under Node rather than starting the pinned Bun app, exact native missing-context error bytes remain unverified; do not pin either wording as the production response without runtime-level evidence. Normal host context is required; missing name/provenance stores are not converted into an empty list.

## Suggested minimum typed facts

Use a presence type that preserves the JS distinction needed for `??` versus `!== undefined`:

```rust
enum JsonPresence<T> { Missing, Null, Value(T) }

enum AgentTaskKind { LocalAgent, InProcessTeammate, Other(String) }

struct AgentTaskFact {
    source_order: usize,             // Object.values(taskRegistry.all()) order
    kind: AgentTaskKind,
    status: String,                   // terminal set is only completed/failed/killed
    local_agent_id: JsonPresence<String>,
    teammate_address: JsonPresence<String>,
    resumable_agent_id: JsonPresence<String>, // Missing differs from Null for MAt
    row_description: JsonPresence<String>,
    spawned_description: JsonPresence<String>,
    local_agent_type: JsonPresence<String>,
    spawned_agent_type: JsonPresence<String>,
    parent_agent_id: JsonPresence<String>,
    spawned_parent_agent_id: JsonPresence<String>,
    teammate_name: JsonPresence<String>,
    is_idle: bool,                    // only exact true matters
    awaiting_plan_approval: bool,     // only exact true matters
    finalizing: bool,                 // only exact true matters
    keepalive_reasons: Vec<String>,
    team_name: JsonPresence<String>,
    is_backgrounded: bool,
}

struct AgentListSnapshot {
    rows_in_source_order: Vec<AgentTaskFact>, // include other task kinds for GTe
    name_registry_entries: Vec<(String, String)>, // (display name, stable id), in iteration order
    hook_caller_by_stable_id: HashMap<String, JsonPresence<String>>,
    inactive_teammate_addresses: HashSet<String>, // exact isActive === false from team files
}
```

The reducer must derive stable teammate ID by nullish coalescing (`resumable_agent_id` unless Missing/Null, else raw address), reverse the name entries in iteration order (last name for repeated stable ID wins), run dedup on source-ordered rows, then compute `PAt -> MAt override -> idle-only GTe promotion`. Output fields with JS conditional spreads (`teammateId`, `parentId`, `spawnedBy`, `name`) need `JsonPresence` so Missing omits while Null serializes as null. Preserve arbitrary string statuses rather than narrowing them to a three-state enum.

For spawn, keep the actual outer result facts distinct: denial/error, resolved model, loop `agentId`, and optional string `teammate_id`. Named teammate provenance is `hookCaller`/`hookOrigin`, and `c_o` bridge facts need the ordered matching task rows so first-nonterminal/last-terminal selection remains reproducible.
