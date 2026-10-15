# Native Client state token and global render-version producers in 2.1.289

This note maps the `ui.render` version input used by Native Desktop Client
fault tracking. It is a static source audit of the pinned Claude Code 2.1.289
macOS arm64 executable, not a live Native UI run. The accompanying probe checks
the executable SHA, exact byte-window SHA values, and the producer call sites.

Run it with:

```sh
node scripts/tests/mods_ui_client_state_token_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The binary SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
All source ranges below are zero-based, half-open byte offsets in that binary.

## Token inputs

Native initializes `renderVersions` as a map-backed store and names the global
version key `ui.render` (`Fv`). `kmn()` calls `E1t()` without a plugin, so it
increments that global key. `bh(key)` applies an immediate map update from
`old ?? 0` to `old + 1`.

For a site, the version signature is assembled as:

1. The current global `ui.render` counter.
2. For each currently matching `ui.render` plugin, its name and the counter for
   that plugin's site pace (`steady` or `live`), in the matched plugin order.
3. The site's `\0state` render-invalidation counter when it has committed at
   least one `$.state` read. The site key is `(surface, component, requestId)`;
   the counter advances after a relevant state mutation invalidates that site.

The implementation spells this as `EUe(state, site) = DY(Rh(state,
uUe(site), cS(site)), state, MD(site))`. `uUe` selects loaded modules matching
the `ui.render` event and all three site fields. The signature includes plugin
names as well as their counters, so adding a matching plugin with counter zero
or changing the matching order changes the signature too. `cS` uses the live
pace for terminal `AbovePrompt` while enabled, terminal `PromptHint`, and the
currently shown terminal `Pane`; other sites use steady pace.

`Rh` explicitly excludes the separate viewport key (`ui.render viewport`),
plugin live-counter entries, and state-key entries from its no-site aggregate
subscription signature. For a site signature it reads the global counter and
the ordered matching plugin names/counters; `DY` appends only that site's
state-invalidation counter. The viewport store is therefore a separate
invalidation input and is not the `Fv` global counter.

## Global `Fv` producers

The binary contains one `renderVersions` store initializer and one getter. Its
only two direct `HD().setState` call sites are the shared version incrementer
and the state-reader site-version updater. A full binary scan finds four
`kmn()` calls: one inside the shared Hooks cache invalidator, two after async
Hooks table resolution, and one in the directory-rehome path.

| Native operation | Source behavior | `Fv` effect |
| --- | --- | --- |
| Hooks loaded-set commit (`J0t`'s `xe`) | After replacing the loaded module list, calls `Hae()` when the old or new loaded list is nonempty. `Hae()` calls `kmn()` before invalidating prompt-related caches. | One global increment on a committed nonempty set refresh/replacement. |
| Plugin unload (`FU`) | Calls `Hae()` after changing the loaded list, then awaits `Kae` table resolution and calls `kmn()` again. | Two distinct increments, one before and one after table resolution. |
| Plugin reload (`MLe`) | On the success path, waits for `Kae`, replaces the loaded module entry, then calls `Hae()`. Refusal/error paths return or throw before that commit. | One global increment after successful table resolution and replacement. |
| Hooks worker respawn (`sBt`) | Rebuilds the loaded modules, calls `Hae()`, awaits `Kae`, then calls `kmn()` again. | Two distinct increments around table resolution. |
| Working-directory rehome (`mit`) | After path/settings/skills rehome work, calls `kmn()` before emitting `tengu_cd_command`. This is not gated on `freshSession`; a rollback path that throws earlier does not reach it. | One global increment when the rehome flow reaches that point. |

`Hae()` also invalidates prompt caches, but its version effect is the global
`kmn()` call at its start. `ui.invalidate('ui.render')` is a separate route:
its Native bottom calls `E1t(plugin, 'steady')` and `E1t(plugin, 'live')`, so it
changes the matched plugin counters, not `Fv`.

There is no timer/debounce in `kmn`/`E1t`/`bh`: each reached call increments
the stored global counter. The unload and respawn paths intentionally reach
two increments separated by an `await`; no source evidence here says their
store notifications or downstream React work cannot batch. The probe reports
the counter writes, not renderer scheduling.

## State reads, folds, and render sampling

`$.state.get` records the current key version in the in-flight drawing scope.
After a non-aborted, identified `ui.render` completes, Native commits that
scope's read set. If a key changed while drawing, or a later state write
touches a committed read, Native marks the affected sites stale.

Ordinary stale-reader notifications use one pending fold timer per pace. The
steady fold is 100 ms; the live fold is 34 ms. The timer batches the stale site
set for that pace, increments the shared site-version counter once, and
notifies stale listeners. `state.set` is special: after updating the value it
flushes both steady and live folds immediately. Plugin `ui.invalidate` has its
own 100 ms steady / 34 ms live pacing; live delivery depends on an active
interactive site and is suppressed for a plugin recently marked slow.

For drawing, `jBr.uiRender` computes `Lue(site)` before starting `FBr`. `FBr`
executes the hook chain and commits the read set after completion. The Client
fault registry starts its snapshot before `uiRender`; after the result it
records the validated Client rows against a recomputed current signature.
Cache reuse checks the stored composite signature against a fresh `Lue(site)`.
Thus the relevant comparisons are at draw start, commit/record, and later
reuse; the source does not define the Host's token wire encoding.

The separate terminal viewport producer debounces column changes by 100 ms
and updates `ui.render viewport`. `Rh` excludes that key, so it must not be
substituted for the global `Fv` input.

For the slow-plugin suppression used by both pacing paths, the cache marks
plugins slow after a cached live evaluation exceeds 34 ms (`U2`), or an
aborted evaluation exceeds 16 ms (`W2`). Either mark lasts one second. The
first uncached evaluation does not run the successful-render slow check.

## Host mapping and remaining gap

The Harness Host now carries a global Client render-generation input alongside
its committed state dependencies and matched-plugin versions. A successful
`ModHost` load/replacement advances it once; unload advances it at removal
start and again after a successful worker reply and local cleanup. Each
tracked UI render site folds a changed global generation into its numeric
`client_state_token` when the token is observed at render start, render finish,
or fault dispatch; the token is returned only when the validated tree contains
a prepared Client module. Global bumps also invalidate all currently tracked
UI render sites, including sites whose matching plugin list did not change.
The actual Orchestrator `CwdChangedFirer` calls this Host input after the
working-directory change and CwdChanged hook dispatch.

The Host's plugin render-version counters are now keyed by `(plugin, pace)`.
`$.ui.invalidate('ui.render')` uses the Native 100 ms steady / 34 ms live
throttles: a steady bump also updates the live counter, while a live-only bump
is admitted for an active live render context or a currently tracked live site.
The worker passes a private `uiRenderReadScope` for `state.get` and
`ui.invalidate('ui.render')` during `ui.render`; it includes the validated
site, Host revision, and on-screen bit and is never part of the Native control
request. Terminal `AbovePrompt` and `PromptHint` sites use live pace; a `Pane`
uses live pace only while the Host has its on-screen signal. The Host currently
classifies every terminal `AbovePrompt` render as live, while Native `cS` gates
that component on its inline-UI enabled predicate; Harness does not yet thread
an equivalent predicate here. Native also compares `Pane` request id with its
shown id, while Harness consumes the supplied on-screen signal. These two
site-activity mappings remain producer-dependent rather than proven equivalent.
The current terminal `ui.render` normalizer accepts only `AbovePrompt`, so this
Host path does not yet produce Native terminal `PromptHint` or `Pane` render
sites. The fake-clock pacing tests exercise the fold and throttle logic; they
do not claim those missing site producers.

Committed state reads are invalidated through separate steady/live stale folds.
A read-set mismatch discovered at render finish is paced and batches with that
fold's pending sites. A successful `state.set` stages affected readers and
flushes both folds before its API reply; an `ifVersion` miss also flushes
already-pending stale readers, matching the Native API bottom. Viewport values
are not stored in this signature, and changing only `viewport.columns` keeps
the Client state token stable.

The Host has no loaded-hook matcher snapshot for the Native active-live test
outside a render call, so it uses the current scoped render or last tracked live
site as the available activity evidence. The worker reports the evaluator's
elapsed wall-clock milliseconds around the matched `ui.render` chain. Only a
repeated live render whose worker-measured duration exceeds 34 ms marks its
participating plugins slow for one second; this excludes the Rust-side IPC
round trip. Native's abort-only 16 ms slow mark is not mirrored because the
current Host dispatch error does not distinguish signal abort from other
failures; this remains a timing gap.
Load failure before a successful worker reply does not bump. An unload that
fails after removal-start keeps its first bump and does not make the
post-response bump.

The current Mod Host has no automatic main Hooks-worker respawn path: a process
exit surfaces as `worker exited`, so Native `sBt` has no Host lifecycle event
to map yet. Do not use Client worker remount epochs or ordinary render
revisions as substitutes. The CwdChanged wiring covers the existing shell
working-directory event path; it does not prove that the separate Native CLI
directory-rehome command is implemented end to end.

The probe is static source evidence only. It does not execute Native plugin
load/reload, cwd change, terminal rendering, or Desktop surface transport.

## Pinned source windows

The executable window hash values checked by the probe are:

| Range | Source | SHA-256 |
| --- | --- | --- |
| `[183434130,183434270)` | `renderVersions` store initializer | `1e746e1c6ea71905e3193763daaf8affdf42704721cf65e4d92c50b42d0ea3bf` |
| `[186021995,186022276)` | `Fv`, `ui.render`, matching plugin selector | `9b29270db93e83f16b29724811a4f32d62065c86b40f42ecbb72d1841e652057` |
| `[186022313,186023106)` | active site and steady/live selection | `ef1b2030c1d8fece3ef884d7d1f6c5ce604841ecc143671bb8623e5330289c07` |
| `[186061240,186062228)` | version map incrementer and paced plugin invalidator | `2b0a4a4e5a053c37291c4c80c2083fc5e95298c0c050b6af7cbd078c5389ac18` |
| `[186062228,186063325)` | `kmn`, viewport key, `Rh`, `DY`, site key | `818a8d15c7accdf4a9fa38d1e601af04f57c248552408b3a81743140edcb0d0f` |
| `[186063325,186064944)` | stale-reader fold/timer implementation | `c877749da950b7d0ec54d79c8ff715ae9ac08f143aaf0341f72ff9fa2435f715` |
| `[186064944,186066294)` | state write versioning and read-set commit | `4244600ee4db9e46f14f4b072a7d6a3e90164e356a3356aa7d55d96d2919c481` |
| `[186067819,186068250)` | `state.get` and `state.set` version capture | `1c54837cc7b798763e4c4a20e4f262dc8139c9a0288d32b2cb9c172cbfe7f505` |
| `[186071887,186073154)` | `ui.render` execution and read-set commit | `74b94db6a3964c9377946bb48c275312ec47b099ae1c6a743a01b5a109c3db65` |
| `[186073154,186073490)` | composite `Lue` signature | `c4ec4302355b4d7c23103b8f1826d441fb6ebda69fefa165ce4c38e3c6cb9232` |
| `[186073490,186074296)` | render-cache store and captured version map | `fdd80ca8e71ef804b387adcb6d5039eb157afe2094be29a2ba20fb54921e0c8b` |
| `[186074296,186075934)` | render cache sample and reuse validation | `b390f9f8b83d4c13b5b572426358a958a0a7ca358867e5a4e7c03a90032f7e23` |
| `[186076108,186077163)` | Client fault token begin/record | `02fe14d3b10c7698a6299396888f2217e58a64025df9a53bfd7085395daba342` |
| `[186098500,186099268)` | Client wrapper begin/render/record order | `bfe3b1722480f6203de10c29e765bf364284bbacb803d0439731d38c4f37ad8e` |
| `[197876875,197877633)` | `ui.invalidate` and immediate state-set folds | `9f22c811fb492a717b0a185369dbe6fb517aa3a2e30276255a547623c0cdf85c` |
| `[188916757,188916837)` | shared Hooks `Hae()` global bump | `9dec22c0f6f9e9bc8a351d107dc4881cc6a1f818b214d4ea63278dad92406e0b` |
| `[188928006,188928329)` | Hooks table-resolution helper `Kae` | `edefc78a7f904814f965bc3fe3a2821b53a77830a1e03fcb6b2d131c652f9615` |
| `[188928329,188933129)` | loaded-set commit | `3e0ad960ad3e4c8be21ea782be73d859de46288a0caae4441f7fc6a397d44c4c` |
| `[188933129,188933775)` | unload and post-resolution bump | `4fa36b175ced8f8d083325a9a4d768ad5118bc618bf67d3427b37ad87f3012b1` |
| `[188933775,188939027)` | reload commit | `653f36774275e1acba95dcba4df1997c971a2d5d7bbc35e378889e720143b211` |
| `[188939027,188941084)` | Hooks worker respawn | `3c236afca7001d683520db42db6e697d99cdbda47c6e15d122276c16d604ac3e` |
| `[209909018,209909442)` | terminal viewport debounce | `76bea93643ad98d3130e2f92f7f9114eae31c59a7c82fb22349cd91bf2a57bb3` |
| `[215372065,215375532)` | cwd rehome and `kmn()` | `f4382e5acc5163da653fc48d54ee0c1b96a54fec238075346dda941c1d9117bf` |

The probe verifies the global call offsets, the `renderVersions` identifier
occurrences, and the byte-window hashes before it prints its JSON report.
