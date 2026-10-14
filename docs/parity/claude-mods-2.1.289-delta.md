# Claude Code Mods 2.1.289 delta

Implementation status: pending unless a contract explicitly says otherwise.
This source audit extends the integrated 2.1.288 Mods baseline; it is not an
enabled Native Mods end-to-end result. Branding, multiple LLM providers, Fusion
and Create Local App remain authorized differences; current APIs only.

The source binaries are pinned independently:

- 2.1.288: SHA-256 bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750.
- 2.1.289: SHA-256 03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69.

Byte ranges below are zero-based, end-exclusive binary source slices.
The official [2.1.289 release notes](https://github.com/anthropics/claude-code/releases/tag/v2.1.289)
provide release context; source spans establish the narrower contracts.

## Agent identity and list

agent.spawn already exists in 2.1.288. Its wrapper validates a nonempty prompt
and returns model with optional agentId. The 2.1.289 wrapper additionally returns
teammateId from teammate_id. A teammate has a stable agentId based on
loopAgentId or a task-registry lookup; retain the separate teammateId.
Provenance and hook events use that stable identity. Do not treat this as a new
generic spawn API or use the bare teammate_id as the universal agentId.

agent.list selects local_agent and in_process_teammate rows, deduplicates by
stable id, and adds optional teammateId, parentId, spawnedBy and name. Its
status projection uses approval waiting, idle, finalizing, keepalive, paused,
storage membership and related background tasks. Other underlying statuses
can still pass through; waiting/idle/running are not a closed enum.

| Contract | Version / byte range | SHA-256 |
| --- | --- | --- |
| Existing spawn wrapper | 288 / 182557687..182558341 | ad5e1d17f9f208a40aa575d67b9ca43925b508ef27656c9f314c76e51e6f4cc4 |
| Expanded spawn wrapper | 289 / 182844300..182844999 | 1f639d650c75229f9e9ebac6cf50ae7694bb4e6fea77fec2763158c8966b47a7 |
| Teammate started identity | 289 / 194834827..194834967 | 75bf8c374b86a0ef46b8c176bcb6265601b21e73b8ee354012d28bab7e113d84 |
| Spawn bridge registry resolution | 289 / 188857756..188858131 | 40d56052b307904e32d76b790f1c7a9bd8434863cf3fda9b7cb00700cad99ced |
| Provenance by stable id | 289 / 188963827..188963992 | b4a9186fc470ab1b5ad28b53ca6d5d8856ec7e8506c36d2bd8f562f18e339259 |
| Old task status projection | 288 / 188204136..188204642 | 249e2dbeb1bbaf7deba2ff6a9a1802aaba5357cd107f46087289c13cfcbe3611 |
| Status helpers PAt/MAt | 289 / 188506311..188507089 | d36381136a0e262d37de6838e1d3d623451a01899a02a2d78b43c6f512691a9e |
| List constructor ifo | 289 / 188507089..188507928 | e003640923c46f03788a530b61ae435e10be9c2ba9dbe9713195b5257d6cba15 |

## UI fault and instance invalidation

The 2.1.289 control request and the plugin hook envelope are distinct. The
internal ui_client_fault request requires plugin, component, instance_id,
client, module, subtype, phase and reason. Its five address fields have no
optional marker; plugin, instance_id, client and module are strings capped at
256, component is a 15-name UI enum, subtype is ui_client_fault, phase is
load/render/run, and reason is a string capped at 200 with empty allowed. The
request has no surface field. The separate ui.fault hook pin preserves exactly
surface, component, requestId, element, module, phase and reason; its check is
a no-op and it adds no field-level enum or length validation. Plugin is the
dispatcher scope, not part of the hook fault object.

The producer emits surface desktop, the parent ui_render component and
instance_id as component and requestId, the Client key as element, and the
surface-module path as module. The Client's nested drawing id appends
NUL-client-NUL plus the key, but that derived id is not the fault hook's
requestId. Phase comes from the Client lifecycle: mount/load failures,
render/tree failures, or held listener/timer failures.

Currentness is checked in two different places. Before dispatch, WX requires
the exact Client identity (component, requestId, plugin, module, key) to be
present in the current registry; a missing Client is dropped without calling
the plugin. The registry ignores results from an older render generation.
After that precheck, dO awaits the plugin-scoped ui.fault chain plus core. Only
in dO's finally does cO re-check the still-drawn site and whether a matching
plugin listener exists before applying local invalidation. Thus a Client
unmounted while the hook chain is pending was already dispatched but gets no
invalidation. The sO version predicate suppresses a duplicate invalidation
only when faultRuns has recorded both bornAt and the current Lue version; it
is not a general stale-version gate and does not drop the event before
dispatch. A version change alone is not enough to prevent dispatch or
invalidation while the same Client remains drawn.

The frame's rejection-to-text conversion reads object.message, stringifies
non-string values, and falls back to “a rejection that could not be read” if
reading or conversion throws. The Client promise wrapper adds its own
“a promise it returned was rejected: ” text before final normalization.
Normalization replaces control characters, lone surrogates and the reserved
sentinel with spaces; it removes one leading plugin prefix and one leading
Client module prefix, replaces U+2028/U+2029, substitutes
“the module failed without a message” for an otherwise empty reason, then
caps at 200 UTF-16 code units. Its truncation helper avoids leaving a split
high surrogate. The Node VM cases preserve this distinction: an empty
rejection message retains the promise-wrapper text, while a direct empty
reason uses the fallback.

An invalid tree returned by an ordinary ui.render hook produces a warning and
the engine draws its own tree. A Client module's invalid returned tree instead
enters the Client render-failure path and may report ui.fault. These sources
do not establish that a generic outer Text/row drawing error is a Client fault.

The system/ui_invalidate schema's optional instances list is internal; the
observed uO/xS path performs local invalidation. It is not evidence for adding
a public stream-json fault or ui_invalidate frame.

Run the source-pinned oracle with the verified 2.1.289 executable:

```sh
node scripts/tests/mods_ui_fault_289_oracle.mjs \
  /path/to/claude-code-2.1.289-darwin-arm64 --check
```

The oracle fails closed on a missing or unexpected binary, changed byte range,
or fixture drift. Its generated source record is
[mods-ui-fault-2.1.289.json](mods-ui-fault-2.1.289.json). It executes the
extracted Native reasonText, promise-rejection Error wrapper, Error-to-text
helper, normalizer and UTF-16 truncation functions, along with the extracted
current-Client registry, WX producer, dO dispatch, and cO/sO/uO invalidation
path in Node VM. Version lookup, listener enumeration, awaited hook
completion, and the invalidation effect are injected host seams. The oracle
therefore proves those source functions' behavior under the supplied seams,
not a complete installed plugin, real terminal renderer, live session, or
public stream-json path.

| Contract | 2.1.289 byte range | SHA-256 |
| --- | --- | --- |
| Scanner hook allowlist | 182596971..182597657 | 4116461f149f4f7e474ad14185870e921cd99ff69c69be0ad344e3d62bdbdec3 |
| Client component enum | 181077075..181077472 | 9fb3958e6ae4fe5b1acb3c594e8858a119e7aec0c098c9fa4ef20946334ab85e |
| ui_client_fault shared address | 181095164..181095854 | 076fe7797a979dda92a631dc2b486d4990ce165edaaf8a7809b7e0cc9c5ffd71 |
| ui_client_fault request schema | 181100761..181102273 | 693fae4140b465ae7bd1fdf6e5e4aa3a6b67ea13b9cb9313148967c4b249e6f1 |
| ui.fault pinned hook fields | 182668805..182668977 | fea23ce0f7ce1aceec80f0b3342f89eb97505eca0a841c2fc3afd6fd842e72eb |
| Current Client registry / generation guard | 186075934..186077175 | 7b9385ffb1f7026da547ae4ef1f10e863e486009390d0dc12a691e25c7f7bddf |
| Draw gate and duplicate predicate | 186077175..186077601 | a3f09f2325ca4b87ff367024b4ccfd899dc975306da6a0fba46397b0fd50ce1c |
| Matcher, dispatch and post-dispatch invalidation | 186077601..186078649 | 8ddc67706bf42056bb3c4d9915365e3185328e1983a69af629b2266b92cefa19 |
| Reason normalizer | 186078649..186079128 | c1cc13647de907da30923899317af7d21826d633bee2be69dd71007b494862f8 |
| WBr/WX producer | 186079128..186079534 | fc8b8c3dfcade48ccf4d0fd5d8b3b0366df6acafe7311abe6cd46d112cced267 |
| UI fault callback attachment | 186099249..186099267 | 9b61783615300917f9d0cb4f282b1f557a4029bf3aa994d3db94fa8088443e01 |
| Client reasonText source | 78987945..78988248 | b63a7ae94eed893416674e2cb0f6c4317ba12e2d63472158447ff24522084702 |
| Client promise rejection wrapper | 197824868..197824944 | 7a8a9fcb748ef4e7f93b79c21cafb45a93f0256a857ee3227b232db5428744d1 |
| Safe UTF-16 truncation helper | 179171341..179171483 | 0cd9c06e168964b38d4d40e576fb3ecc8b21f8d0f864efa607f0f0fec619afe7 |
| Control/lone-surrogate scrubber | 182639750..182639987 | baec57e91c2bc5d5447f2635a7fb24553b4204f964f2f39afe5db4bfe65efd1c |
| Ordinary ui.render invalid-tree fallback | 186072521..186072886 | f335b3328b9f021349a3ca9b242f6b0b75d721102f93a660e80c09ad36865f08 |
| Internal ui_invalidate instances schema | 180875737..180877101 | 01a251bab24d3f9bee03259064fadf0abb3b2f7719e5edde016164a7b1d60751 |

## Other changes and acceptance limits

The recursive same-behavior nested permission-rule lookup has already been
implemented locally and source-probed; see [the permission evidence](mods-permission-2.1.289.md).
Harness still lacks the corresponding native compound-Bash result-tree
producer. Synthetic nested projection tests do not prove that production path.

The release notes also identify two sandbox auto-allow Bash regressions
involving environment assignment prefixes and bare assignments before commands.
Both versions already contain related shell-parser AST logic. The exact changed
auto-allow consumer remains to be pinned; do not label an unchanged parser as
the new fix. First-upgrade session Mods loading also needs a source-pinned
loader path and a regression fixture. No Mods-specific prompt-cache key or
schema delta was established by this audit.

Required next gates are stable teammate identity through spawn/list/hook
events, the full list status table, plugin-targeted UI faults with stale-site
and partial-redraw tests, and native raw event/result/error bytes. The existing
2.1.288 Mods matrix still contains additional API, scanner, lifecycle and UI
gaps; this delta does not replace or close those rows.

## turn.step pre-request buffering

Both versions buffer assistant items until a physical request starts. A
pre-request hook failure discards that buffer and retries the original query
outside the hook. A pre-request abort discards it without a retry. Successful
synthetic-only completion flushes it and marks answeredWithoutRequest. Once
the core request counter has been incremented, an error does not trigger this
retry; the wrapper flushes its pending items and propagates the failure.

The integrated Host repair preserves this split across the Node worker catch
and the agent iterator. The parent-library regression results are recorded in
the [integration receipt](claude-mods-integration-validation-2026-10-03.json),
with their separate source snapshots. These source spans and local tests do
not constitute an enabled Native CLI run.

| Contract | Version / byte range | SHA-256 |
| --- | --- | --- |
| Prefix buffering Jn | 288 / 194349726..194350073 | 6a7a1c3f3dd4560764fd01c88ea123be47c9ce8e0cabf7d9528dd6600bd22b14 |
| Prefix buffering Jn | 289 / 194663353..194663700 | e53c0f577338006077256b7be23c270065ed8464161f7b3ca115a8135b10fab4 |
| Retry wrapper vs | 288 / 194361391..194362112 | 2fe5b0b1e9914d81d1410e36a2117f980124c849dc19e41f88b0d8dbb2fe3a13 |
| Retry wrapper Ms | 289 / 194675012..194675739 | 5b877b435cc047bce013eeeb572341428630bc6de3e432ba6b3675983532218d |
| Physical request entry Ts | 288 / 194360617..194360979 | dce4355fd0c745ef5b6ea8730ee79ec500fe60540bd8da8790b2f3dfaa24819e |
| Physical request entry Ss | 289 / 194674244..194674606 | 94071c97baa2e81c1d949fb50343b7269d7f1508f836af9870069e410f6174f3 |

The abort-helper conditions and SDK abort subclass are also structurally
unchanged; the identifiers differ. This conclusion is a narrow source audit.
No live request, VM comparison or full Mods lifecycle success is inferred.
