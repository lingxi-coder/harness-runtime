# Claude Code Projects consent lane, 2.1.289

This is a narrow source audit of the Projects-session `$.tool.call` consent
gate. The pinned Native input is the official Claude Code 2.1.289 macOS arm64
binary, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
The [2.1.289 release](https://github.com/anthropics/claude-code/releases/tag/v2.1.289)
provides release context; this note records only contracts confirmed from the
pinned binary. Source offsets are zero-based and end-exclusive.

Re-run the extracted Native source cases and fixture check with:

```sh
node scripts/tests/mods_projects_consent_289_oracle.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64 --check
```

The oracle writes and checks [the source and result record](mods-projects-consent-2.1.289.json).
It fails closed on a missing binary, a different executable hash, changed
source slices, or fixture drift.

## Native gate

The `Uoe(tool)` predicate applies only to `WebFetch` and `WebSearch`. Its
Projects-session base signal is the OR of three separately owned facts: the
default host's sticky `Is(el).value`, parsed `CLAUDE_CODE_PROJECTS_SESSION`,
and a matching MCP client in the active session (`Sbt(SS())`). If none is set,
the Projects restriction is inactive. When active, the only exception is a
feature result with both `value === true` and `source === "payload"`, while
GrowthBook's `Use()` reports no prior use of a non-default API host. Every
other active-session case rejects a supplied consent field. Missing consent
skips this specific rejection and proceeds through ordinary permission; a
present `null` fails string validation first, and an empty string is still a
present value.

The GrowthBook source discriminator matters: `disk`, `fallback`, `disabled`,
and `override` are not `payload`. This binary's normal `Fu` client getter
returns `null` environment overrides and no config overrides, and the override
setters are no-ops. The Native getter's payload/fallback/disk/disabled result
branches are executed in the oracle; a remote evaluation map and disk snapshot
are explicit fixture inputs. GrowthBook's default API host is fixed to
`https://api.anthropic.com/`; the separate `Use()` bit belongs to a per-session
GrowthBook client. The source shows the ordinary default-host path leaves that
bit false. A host must not substitute an LLM provider route for this fact.

The gate's source and consent argument boundary are pinned here:

| Contract | Byte range | SHA-256 |
| --- | --- | --- |
| Typed bool parser `Le` | 179188527..179188669 | `c7c851b23895c896ce38865f13ecbb54e11e83018c8ced9cae422c41298e6dfd` |
| `CLAUDE_CODE_PROJECTS_SESSION` schema binding | 179490117..179490128 | `2a8dcb5c9721fdaa241462558f0509f6df118060b29c989566c381231063bf64` |
| Default-host/live-session accessors `U/n/Is` | 179291610..179291705 | `339f41d1136cbf3482b3ae3286ce29b937fe29410b48a1180fb6df732bbd2298` |
| Current-session MCP clients `SS` | 179323539..179323603 | `d801a4d382bec9dbffd44bba081be565dd6ea7effdb77ebde4feadb913264522` |
| Native hearthbot predicate `gzt` / `Sbt` | 181808884..181809071 | `a1c25e2e69a503d209add1b9b535b208d07be2df74425913a18a90b79b48c6e2` |
| GrowthBook API host constant `Nu` | 181836102..181836162 | `e364071f5633d829ba6a0f3bc20f472801f4273f5d0e890cb410af584f785924` |
| GrowthBook per-client host-history initialization | 181836572..181836597 | `119dc1c25ca51b86a8768e5cdd588472e0af12fb443488cb962e73aa30128dfb` |
| GrowthBook host-history assignment | 181844245..181844304 | `ff3f1dfaab8de2deb0652d1ef73120a16578edf781006d622abcad595b8cb0b2` |
| GrowthBook value/source getter | 181846341..181847084 | `106782695c7016daf255548f5737e6472e2bb5bb1b860e58d9eae8b253220113` |
| GrowthBook default-host `Use()` | 182189510..182189561 | `283c216316d0e34b03db2d231144a94646c9b7f587573fcb043814102177e92b` |
| Projects predicate `Uoe` | 183805687..183805858 | `7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb` |
| `$.tool.call` consent guard `eU` | 188853212..188855653 | `a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66` |

## GrowthBook enablement, payload and cache lifecycle

The `Fu` dependencies are composed lazily by `Ne()` from the current host's
`ui()` store. The relevant dependencies are `isEnabled: th`,
`isDiskCacheReadableWhileDisabled: NU`, `readGlobalConfig: le`,
`saveGlobalConfig: Ae`, and `fetchRemoteEvalFromSdk: IU`. `th()` is
`!DISABLE_GROWTHBOOK && !Ih()`. `Ih()` blocks ordinary GrowthBook evaluation
for a non-first-party provider unless the provider is marked managed by the
host; it also blocks when gateway auth is present, telemetry is not in its
default mode, or a custom OAuth URL is configured. `He()` classifies gateway
auth/server/host-policy as `gateway`, then the Bedrock,
Foundry, Anthropic AWS/Google, Mantle and Vertex modes, otherwise `firstParty`.
This predicate reads provider/host-owned facts; an LLM base URL by itself is
not a substitute for it.

`YL()` is true whenever `Ivt()` is not `default`. `Ivt()` returns
`essential-traffic` when `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is
truthy; otherwise `DISABLE_TELEMETRY` or parsed-true `DO_NOT_TRACK` yields
`no-telemetry`; otherwise it returns `default`. Thus ordinary non-default
telemetry policy disables remote GrowthBook evaluation, while the separate
first-party-only disk-cache exception can still expose persisted values when
its explicit boolean flag is enabled.

The separate disabled-cache exception is narrower. `NU()` is true only when
`CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF` is true, GrowthBook is not
explicitly disabled, telemetry is non-default, and the provider is first-party.
It permits the synchronous feature getter to read persisted feature values;
it does not enable remote evaluation. Both `DISABLE_GROWTHBOOK` and the
disk-cache flag are boolean typed-environment fields in this binary.

For the source-aware getter, environment/config overrides come first (the
current Native `Fu` methods return `null`/`undefined`). Then the disabled gate
returns `source: "disabled"` unless the disabled-cache exception applies. A
key present in `remoteEvalFeatureValues` returns `source: "payload"`; a null
value falls back to the call's default while retaining that source label. If
the remote map is nonempty but lacks the key, the getter returns
`source: "fallback"` without checking disk. Only an empty remote map consults
`readGlobalConfig().cachedGrowthBookFeatures[key]`; a defined value returns
`source: "disk"`, and a missing key or read failure returns `source:
"fallback"`. Thus Native `source: "payload"` means a key exists in the current
accepted in-memory remote map; it is not a copy of the GrowthBook SDK feature
object's own `source` field. The reducer also puts `source: "defaultValue"` and
`source: "unknownFeature"` values into that map when they have a value, while
excluding them only from `nonDefaultFeatureKeys` bookkeeping.

`processRemoteEvalPayload(client)` is the actual map updater. It reads
`client.getPayload().features`, skips non-object and valueless entries, adds a
`defaultValue` when an item has `value` but no `defaultValue`, and extracts
valid experiment metadata. It calls `client.setPayload(...)`, verifies that
the client is still current, then clears and replaces the experiment maps and
`remoteEvalFeatureValues`. Missing/empty feature maps and maps with no usable
values return before replacement, preserving the previous accepted map. The
oracle executes this Native reducer with a fixture SDK client and executes
`syncRemoteEvalToDisk` against a fake global-config writer; the fixture covers
null values, source bookkeeping, valueless entries, and the empty-map case.

`syncRemoteEvalToDisk` skips persistence when `clientSlackTagConnected` is
true. Otherwise it writes `cachedGrowthBookFeatures`, sorted
`cachedExperimentFeatures`, `cachedExperimentData`, and
`cachedGrowthBookFeaturesAt` through `saveGlobalConfig`. The Native global
config default object initializes `cachedGrowthBookFeatures` to `{}`, and its
persisted-key set contains the feature cache fields. The read dependency goes
through `le()` and the current global-config cache. Its loader uses the host's
GlobalConfig V5 storage when that storage is supplied; otherwise it reads
`A().getGlobalClaudeFile()`. The ordinary path prefers an existing legacy
`.config.json`; otherwise it uses `$CLAUDE_CONFIG_DIR` or the home directory
and `.claude{OAuth-suffix}.json`. This is global user configuration, not a
project/workspace settings layer. No real user config file was read for this
audit.

The owner is one lazy `Fu` client per current host, not the telemetry crate's
process-wide boolean map. `createClient()` returns without starting the SDK
when disabled. With the applicable auth result, it initializes the GrowthBook
SDK (`remoteEval: true`, user attributes and a 5-second init timeout), then
processes its payload. A blocking getter can initiate lazy initialization; the
application initializer can also pass an optional
`startupGrowthBookKick` callback. After initialization, `initializeClient()`
starts an abortable periodic refresh. `refreshFeatures()` handles auth changes
and attribute changes, requests an SDK refresh, processes the accepted payload,
updates global cache and emits the refresh event. Reset/dispose stops timers,
destroys the SDK and clears the in-memory remote map. These request, auth,
startup and refresh paths are source-pinned but are not executed by this
oracle; no credentials or remote request were used.

| Provider/cache contract | Byte range | SHA-256 |
| --- | --- | --- |
| `DISABLE_GROWTHBOOK` export | 179485794..179485820 | `95ca25b27037d79aab08801b767447f980e45f8078e55338cda9db5c96a6c99a` |
| Disk-cache flag export | 179481409..179481461 | `0587b094bda84ee631f9a63ad0f8d1ed9c33f39376305514e3d41ef7c69e9866` |
| Both flags use boolean schema (`Jn`, `Zr`) | 179490508..179490520 / 179488313..179488325 | `39d379eb0dfaf8bb28101e5b6886e66a2df353e7f685fe8a786f35107dde23bd` / `76213ae0ad133fc303b268e81eeede353430fa895bda05dfd3efaf3d87e3bb57` |
| Optional init-kick env export and boolean schema | 179481592..179481637 / 179489345..179489357 | `9f6a9081d20158c7e6a6e87c0e06be390cac47983882cdf3c7e011c2fdd0dd3e` / `cf4f120a966ce3c81bee060b2f9150025000f73de9271d7db1d955e4bf8f287f` |
| Telemetry mode `Ivt` / non-default `YL` | 179441125..179441433 | `1d399b4e227889efaf535677cf7a7acb7f10ef254c6a98eaf8b5988e01ef492c` |
| Provider/host/custom-OAuth gate `u` / `l` / `Ih` | 181601954..181602237 | `7c491381d0b6eabd839834a9344348bf588aeaab3f091e73b7209e6adad985b7` |
| Provider classification `He` and first-party predicate `Jn` | 180332633..180333068 | `12a78e3b9fe2afef9d10b53f90ca42582f02b958afb695776087c0938d3bb344` |
| Gateway auth presence `go` | 179315397..179315457 | `e9d6f3267ce2d54a5043627a0120b2b6c7bb5d486b23b54ae622c36312e7c39b` |
| `Kz`, `th` and `NU` enable predicates | 182182552..182182578 / 182189752..182189903 | `1cd034bad94b05e88674ab7e1abde363f964a1f3baf9dfae7c2103556e052cc7` / `648fdcadd3707a05a510ddc4d3b1598055012efcfb6ad6c37c28645060174329` |
| Per-host `ui` GrowthBook store and `Ne` dependency wiring | 182188098..182188206 / 182188260..182188797 | `ca7a8653cbd341e6585599fc3e6a3ef0ce5492dbf39e096ff72ddbe422dc8cca` / `a2f1195c3e8d064743039c55d93b4dab33ab5a94f157e3fbc0f43e3a227a7b1a` |
| SDK payload reducer | 181839986..181841899 | `f848a85e6c29b4aeb26c7901962d9e9dfd02fd691e0ec697e44867aa6870b27b` |
| Global-cache writer `syncRemoteEvalToDisk` | 181841899..181842441 | `5966aeb06af717d4089ad62de96a5134fd8d2dd6f6ee4fb33eecb390e4d733a8` |
| SDK remote-eval request/fallback | 181842552..181843404 | `7ad678fa323696e490c84a87cf81d447325e99dcff470bbe2908637919d96b33` |
| SDK client creation and initial payload load | 181843510..181845518 | `ed90daf5bc948ca2456c9b8a77fe0cc281e69d71678c41d81a917151c76d96ca` |
| Lazy initialization and periodic refresh start | 181845518..181845969 | `ce2864e34b1e1cbbc1ee9ed4abf58962837f35d1703c77f47d5fa0b77b353c04` |
| Blocking lookup starts lazy initialization | 181845969..181846341 | `2cd67afb0f3deab2af768f8088a45bdde3ce85dbdf0172b47475be6a2852fcf8` |
| Reset clears remote payload and generation | 181847787..181848858 | `2d03f9b15615977c89a64f3c4683494b97f5fa34668a65ecc9e12d84d5c2f788` |
| Dispose restores SDK remote-eval route | 181848858..181849087 | `d917fe55d45aa1b17adfe5a6fca5b0940dd7406e4de1923b7d10ea03c71b9af2` |
| Auth/attribute-driven SDK refresh | 181849087..181850371 | `abc5e9cb5acb3d7d5d35182698fc94c6f5e32392bc8ad552bc202f8dc6541567` |
| User-attribute-triggered refresh | 181850371..181850631 | `0c148bb4fab9134599150c88691c0e44e2d31abd9a3559c792233f914a75f815` |
| Periodic refresh start and loop | 181850631..181851437 | `f5aa178908a5922d85eef16f0d486b2fb6baabd1a956e8267ac739f5cd4e2c8f` |
| Periodic refresh abort and listener removal | 181851437..181851707 | `43a588792f68910c96a8afaa85fc63fe87accb70bd39340e70f68bd3a2a7b66a` |
| Application supplies `startupGrowthBookKick` from env | 196842027..196842209 | `e6b3075214f846e94b21e3aee6362278a8b957330a918fd48c0a8587c452db96` |
| Init invokes optional kick after network setup | 195707515..195708044 | `f0e7d2cac6036f96034558ad04322428785db38cfa6d9b8958802fd0f44fe9a0` |
| Native global-file path selection | 179459495..179459733 | `ff65b45549fc64764bfa8f6e6205ea27a07c42a043583f6cb18b566dd1482e04` |
| Memoized `getGlobalClaudeFile` | 179464376..179464434 | `d269f927afe6173ac3051158cabd7f7914ed74d9e5ed59c9b6c59ab758aaf838` |
| Current global config cache read `le` | 182239514..182239690 | `3df4808eca365d5ff3e0f0fd53307d0d00ec4630955911c3e3d1905a7bd7ed50` |
| Global V5-storage or file loader `Gzt` | 182240110..182240530 | `9a4b0cbd1bb13201e3ea772dd998ab97eb8e90a23e2a154ce0fcb0875a5133f9` |
| Global config `readCache` method | 182213210..182213309 | `6608e6f53459b7c0a99f115a09c053985ed67d6c5089026e35ee94cc667efade` |
| Global default with empty GrowthBook cache | 182221210..182221992 | `f2010f3f6e9dd283e183e00ac4f08067cf4c0009e0707c7fdc3b2d3d7e808b15` |
| Persisted global feature-cache key list `W0` | 182227939..182228814 | `b92e1fecc00153efc5ffa122279c6bf8802ea0d14617b736ca18368784e711e9` |

The optional startup callback is supplied from
`CLAUDE_CODE_GROWTHBOOK_KICK_FROM_INIT` during application initialization and
invoked by the initializer after network setup. It is an eager-init hook, not
the payload source itself. The initial and periodic SDK calls remain
source-only in this oracle.

## MCP signal and URL matching

`Sbt(SS())` reads the current session's clients and accepts a client only when
its name is `hearthbot`, its config scope is `dynamic`, and it is not marked
CLI-owned. The URL predicate first requires the target and startup URLs to
have the same serialized origin, after changing a target `ws:` scheme to
`http:` or `wss:` to `https:`. It then requires the target pathname to include
one of these paths: `/v2/session_ingress/shttp/mcp/`,
`/v2/session_ingress/mcp/ws/`, `/v2/ccr-sessions/`, or `/v1/code/`. The
hearthbot-specific check narrows that to exactly
`/v1/code/mcp/hearthbot` or
`/v2/ccr-sessions/(?:-|[A-Za-z0-9_-]+)/hearthbot/mcp`, with no nonempty
username, password, query, or fragment. Empty trailing `?` and `#` serialize
to empty search/hash and pass.

Origin comparison is string serialization, not tuple-origin identity. Both
`file:` and `data:` URLs have Native `URL.origin === "null"`; if the target
has a recognized hearthbot path, the extracted Native `qS`/`Jkn`/`Sbt` path
accepts a `data:` target against a `file:` startup URL. The Rust implementation
compares `Url::origin().ascii_serialization()`, whose opaque-origin
serialization is also `"null"`. The fixture executes this cross-scheme case,
plus `ws`/`wss` target normalization and the unnormalized startup scheme. This
is a narrow compatibility observation, not an assertion that opaque URLs are
valid transports for a real MCP connection.

The other Native signal is different: `NSn` sets the default-host sticky flag
when called with a matching non-CLI hearthbot config. Its caller is in the
remote dynamic-config normalization/reconciliation paths. The exact source
pins are:

| Contract | Byte range | SHA-256 |
| --- | --- | --- |
| `Is(el)` lazy host store | 179227685..179227829 | `cee89b130a2f5db0c77c76f307ffc83ab3fa6541e44f33023710e68965bc126b` |
| Native CLI-owned marker `OEe` | 181499037..181499398 | `d17f7d3fcd1028b404f0efb36775837c8c4e881e07ba99cc8e56a0642a79fbec` |
| Native CLI-owned predicate `nf` | 181499398..181499479 | `9cadb46a3609fcbc06e3f765ac5ab0e1fe057f3f1da5e12d6243ac36ad583e06` |
| Startup URL environment selection `C` | 181508045..181508129 | `be0cac9bd887d1a62be3220c574ecf0dcf686cf427eeafc60f9a5339db23954b` |
| Startup-origin/path predicate `qS` | 181502797..181503054 | `ee962826757386373f730209c9ddc69982dd11cf15e854e55630383ac1ee3b35` |
| hearthbot URL predicate `Jkn` | 181506639..181506912 | `34dbdfee8e9c62c21e2a68fb14202aaf7d71c6609dba78d8f684646e097df954` |
| Remote dynamic-config `NSn` setup | 183805484..183805858 | `c07601d85715ef0f4c9ab3e648ffc33812bb1e0e381e456c0727c551c28ea812` |
| Remote dynamic-config caller | 196544676..196544722 | `c4f77132cd9624c389e069983eb4d960b5bd488274c9642586527ebc41c9a03b` |
| Remote refresh/reconcile caller | 205965340..205965418 | `39d067a8ac92c3a53d3891bb2807d55a44ac70e4bd3dc05f839001619ca29343` |

The `CLAUDE_CODE_REMOTE` guard and the calls to `NSn` are source-pinned. The
oracle invokes Native `NSn` directly for accepted and rejected examples; it
does not run the complete remote-config ingress or MCP refresh lifecycle.

## Harness composition status

The current integration has a trusted `ProjectsConsentFacts` DTO in
`crates/hooks/src/mods.rs`; its `Option` fields preserve missing/unknown
separately from false. `OrchestratorConfig.mod_projects_consent` is skipped by
serde so worker JSON cannot provide these facts. At the tool-call boundary,
the orchestrator reads the host latch and live session-client predicate from
the injected registry, parses the typed Projects environment value, and reads
the latest source-aware feature fact when one is available. The desktop
assembly marks the actual `cli_mcp_servers` configs as `cli_owned` before
registry ingestion.

The Harness host context now has explicit startup ownership. Constructing
`McpRegistry` is environment-free and leaves the optional context absent.
`ProjectsSessionHostContext::new(startup_url)` is also a pure constructor; it
stores the already-captured startup URL and initializes the Native default-host
sticky latch to `false`. Desktop composition captures the startup URL once
from `SESSION_INGRESS_URL`, falling back to `ANTHROPIC_BASE_URL`, and injects
the context into its registry. It does not substitute the active model's
`api_base`. A registry without an injected host context reports no host fact;
an injected context with no startup URL has a known-false latch and cannot
match a session MCP client. A completely absent registry contributes an empty
session-client set, whose Native `Sbt([])` result is false; this is distinct
from an existing registry with no host context.

Mobile composition is explicit and currently has no startup URL producer.
`MobileConfig.projects_session_startup_url` defaults to `None`; the Android and
iOS FFI launch DTOs and their conversions do not expose or fill this field, so
their current contexts have no startup URL and start with the Native latch
`false`. Mobile construction does not read process environment. The main
mobile registry and its Local-App child share the same host context.

Two earlier integration defects have been corrected: a generic registry
connect observer no longer treats every `dynamic` MCP server as Native remote
Projects evidence, and CLI-originated configs are now explicitly tagged at
their desktop assembly source. The remaining producer gap is concrete: no
in-repository first-party remote dynamic-config ingress updates the host
latch. The former bare `record_remote_normalized_config` function has been
removed; adding a generic `dynamic`-scope connect hook would not supply the
Native remote-ingress provenance. The other unresolved fact is GrowthBook
source acquisition: Harness has no source-aware GrowthBook resolver, so
`feature_result` remains unknown. When a base Projects signal is active, the
tool-call gate fails closed as unavailable rather than inventing `payload` or
`fallback`.

Harness has two reusable substrates, but neither is already the Native
provider. `crates/telemetry/src/feature_flags.rs` defines
`FeatureFlagsFetcher::fetch() -> HashMap<String, FeatureValue>` and a
`FeatureFlagsClient` with a 300-second refresh loop. Its synchronous
`flag_bool` reads a process-wide map after a test-only override and returns a
default on miss. The map carries no per-value source, disabled/read-disk state,
or session/host ownership. A search finds no production fetcher implementation
or `FeatureFlagsClient` construction in this checkout; the production
snapshot therefore stays empty and current callers use their passed defaults.
It cannot produce `source: "payload"` merely by being given a bool.

`crates/migrations/src/global_config.rs` is the reusable raw global-config
substrate: `global_config_path`, `read_map`, and `save_map` preserve unknown
JSON keys. Its current LingXi path uses `$LINGXI_CONFIG_DIR` / `~/.lingxi.json`
(with the legacy `.config.json` path), which is a different branded config
file from Native's `.claude{OAuth-suffix}.json`. The existing desktop assembly
already resolves this global path, but no GrowthBook feature cache is wired
through it today.

The minimum provider contract for the next integration is a synchronous,
source-bearing per-key snapshot plus separately acquired enablement and
disk-cache-readability facts. It must preserve Native's `payload`/`disk`/
`fallback`/`disabled`/`override` distinctions and defaulting order, and update
the payload map only after an accepted SDK feature payload. The asynchronous
fetcher should own auth, provider attributes, refresh cadence, and shutdown;
the `$.tool.call` check should read its latest accepted snapshot without
starting a network request. Persisted cache should use the host's actual
global-config store and preserve unrelated keys. This is an implementation
contract inferred from the pinned Native path, not a claim that the current
fetcher or global-config code already implements it.

The Node VM executes extracted Native parsers, predicates, feature lookup,
consent guard and host/session stores. Session clients, startup URL, remote
feature map/disk snapshot, fake SDK client, fake global-config writer, and the
physical W1 tool generator are explicit fixtures. The oracle does not run user
config, accounts, real MCP transport, GrowthBook request/auth/init/refresh,
permission hooks, or provider requests. It executes the extracted Native
payload reducer and source selector with fixture data; its source slices and
fixtures do not prove the production network/provider lifecycle.
