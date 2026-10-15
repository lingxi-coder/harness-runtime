# Native `$.tool.call()` effects and frame lifetime in 2.1.289

This note traces the Native `$.tool.call()` path beyond the Promise wrapper. It
records the boundary between tool execution, selected middleware output, host
effects, and the public API result. It does not claim a full Native runtime
execution: the checked-in wrapper oracle supplies a fake `W1`, and the
additional dispatcher probe below supplies a fake underlying tool runner.

Run the checked-in wrapper oracle with the pinned macOS arm64 executable:

```sh
node scripts/tests/mods_tool_call_289_oracle.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64 --check
node scripts/tests/mods_tool_call_ref_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
node scripts/tests/mods_tool_call_agent_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
node scripts/tests/mods_tool_call_preflight_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The pinned executable SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`. The
extracted primary chunk `src_187181803.js` is 4,509,717 bytes with SHA-256
`47b62b9ac561f0cc5cd75ab6e7b9e14c1b027cccebfb900b56f07685c69fa825`; the
session-host chunk `src_206056762.js` is 580,920 bytes with SHA-256
`ca03716821e6b76b1769143dcb5b95654d56a7d1c41036c31e0ed3c1c421c937`; the
tool-catalog chunk `src_183191976.js` is 137,865 bytes with SHA-256
`ce77e5ade76efa7292b949480b9e1bf490ee657ab74f842dcc27783d4ab81281`. Each
range below was checked against the corresponding absolute bytes in the pinned
executable.

## Source map

All ranges are half-open absolute byte ranges in the Native executable.

| Function or expression | Byte range | SHA-256 |
| --- | ---: | --- |
| `W1` tool execution generator | `188814536..188819240` | `2dac343f7459b2386e673fc7726ae690d0b2bb0304ae2e8727fda593fcbaa3d4` |
| public `$.tool.call` wrapper (`nw`) | `182844999..182846132` | `10bc2d7a98eaebd0dcc3193e7dd3c97d524cfac6c5b9415e9c5f8f10e3ca0812` |
| `I9` hook API and event namespaces (`$Vt`) | `189569136..189570377` | `40f00de65c2d558b7413b7d652112c45c3067e86388e997d35df266270275c27` |
| hook event dispatcher (`ya`) | `189567939..189568662` | `7f2174002d84e3674d97eab336fe6eee3795c37843cec14117c88f083a78211d` |
| hook chain signal propagation (`qT`) | `182771210..182771783` | `7acd77cff9cc51eda72d43e33f95191668b2792d35c2e267e78f3abcbc3a73b8` |
| hook handler abort linking (`eg`) | `182774738..182777607` | `b4beb925b40fc51afa13d415d61892c2cf6ea8203154a2c4b2f9649beaa487d0` |
| abort-signal to controller link (`av`) | `182769377..182769578` | `179b1becbb41a9e8fbc818af5a981e52c003a5cf4984ca09fee507525b3614e1` |
| virtual plugin tool-use row (`hfo`) | `188508394..188508499` | `4e933392955334171fd9efc7dd0a1ab917ecebf507c0037e30e6c010318ee6f7` |
| active plugin-session accessor (`Hu`) | `188335054..188335274` | `5b08f0d0c21e64b340856d0f6593c78d9f1ec7b366dd02ccb23c7a1fe5daa9fe` |
| wrapped V1 batch-tool filter (`Mx`) | `183328429..183328612` | `b8fb3141477c690ceee58e51e016579ee9975eff4b095376f8b2632df29fdba2` |
| tool name/alias matcher (`Ht`) | `183324780..183324845` | `b2650546effe0fc34bcc0d1d39749c2957ac76aa753f6df32f3295928399366b` |
| tool catalog alias resolver (`Zl`/`yn`) | `183324955..183325363` | `a179f2564b0982eecb1287d53c2746dbd4f79c2e400d78d49a072f3644288411` |
| active resolved-tool builder (`di`) | `206475564..206477340` | `02cec48c78535528d898d12e9c0a053fffb40c7360d5b1e8285f6c5141046650` |
| current tool-list getter (`Wi`) | `206477340..206477355` | `aae30344138e2b213cebc3062e5c4e6ec168fd5bb28978c0eb526fd2558afd97` |
| Projects consent predicate (`Uoe`) | `183805687..183805858` | `7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb` |
| `jyo` tool-call module and frame queue | `188819240..188822297` | `fc54eb15816019da0ea3b5cb5a390657a18c50c2697f73b6f94187849238518b` |
| `zyo` permission and core tool execution | `188822297..188852695` | `7e973a4e0aacde282507323c9b78cdc5785c2d71ed23dcb3cb69338b4de0fb78` |
| `eU` `$.tool.call()` Promise wrapper | `188853212..188855653` | `a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66` |
| `Yyo` Agent spawn wrapper | `188855653..188856054` | `871a5bd9cefdb815fd357ef4ada0687c23924b4e2c766ec4b8f166806e1ccf87` |
| `UOt` Agent settle/wait result | `188856525..188857230` | `913abc29d97e8f34d5d3654351d6035096175c88363dbf602579da671e0c25c8` |
| `UOt` complete async function | `188856519..188857230` | `1bc56030e87f66744b2c581b49bf60678f36ba8bff3f1e5e40722c0c8fc90bd0` |
| `BOt` launch-only infinite settle wrapper | `188853103..188853212` | `caa26e4acdc771ea59d5ecfdc7ab29d7d447a55ce62bfaa6537b302aa5a7b0ad` |
| `_fo` Agent async-launched ID extractor | `188508499..188508612` | `ad60de1ed98d460840f58b1427f6bc00a15e9b526078b73548a8fbd73e1ec948` |
| `LAt` Agent API result constructor | `188508220..188508286` | `068f137299474ee14942ab54ff5282227f936d9811a7e3f5186d0b852b5c7601` |
| `FAt` Agent/API error result constructor | `188508310..188508356` | `045f105e8a6e8de36b56189cfb4a4a40534f75ce98a68423376f9fd1777cf51f` |
| `ptr` `resolvedModel` string extractor | `182844131..182844217` | `d5e529c1e38c148dd82169f5b3386ed096ffa039e2d3ad397d32047b36e79566` |
| `pae` transcript lookup | `188856054..188856107` | `265154c447ced94580ecce813b4cf513cdaf35a4c3bcd728a99d2fbfd9555e13` |
| `kfo` latest assistant transcript text | `188508693..188508846` | `78e9dd61f073543aea37d66be2c7c85e96afdc91be5b16c04c3f91ad7a513de4` |
| `n5r` text-block concatenation | `182734632..182734762` | `97bf28497dd65200e0a242062325a5432ae5f5f4b5505f4a381a56ad2fc37933` |
| `GOt` evicted-Agent transcript/error branch | `188856160..188856320` | `30d7dd20293bd627587fdf411b5344f3bbb9c238824ce3734a4cf1d3a7475511` |
| `qOt` timeout/abort settlement wrapper | `188856417..188856519` | `9d15eaf1720748a63642208cd3a45cb127d205ee7896717d200da89705343912` |
| `zOt` wait for Agent settle/removal | `188856320..188856417` | `7ec1018cc90e22070084fb7237ab2d5f3c6de157baaa037bd093201a22917cae` |
| terminal Agent status predicate | `185139913..185139979` | `97b7f941c1c09455cb734a557cb33b37fcdd82297be2af30b4d77296dd6ccbcd` |
| Agent poll interval | `188856107..188856119` | `8b4c9339444ed89d503e6cfe22ec521f9d5e8edad93767eb921f874f1c06c334` |
| Agent wait constants | `188856119..188856160` | `d360de8b1c91e3129e5059c5c7cc0c6808ad7210f8156cfbe018e21133a2f3a7` |
| milliseconds factor | `182764660..182764672` | `696b6e3fd67765a5c34d84341b5660ca48be01b9d0a58ae82e6ce4f1f050cc4d` |
| `XIe` run ledger | `182763200..182763365` | `bcb84d348c2772be59766f01ba7d207eca4e1dc1da9ecdec630aa5268f979294` |
| `vUn` pass-through result comparison | `187737832..187737934` | `31df52258cb1479b8ae29ca06ab2990da7383ced46550b116e37c30d3d53dc60` |
| `wUn` permission-denial tool-result block | `187737720..187737832` | `086a383343a8675cd9ece487418c323fa8e3035e071462a6574347d1fece5e4d` |
| `MUn` core middleware run | `187738611..187739221` | `ac07c963162f666d98765743c3183b45e8ed3086d83e1f73745f7c000f33d47e` |
| `RCe` permission-denial row constructor | `187739221..187739325` | `db18518faacc62b346b33e21ad5cf910d069b559026fab8066449ea97512af77` |
| `ma.check` public `tool.call` result validator | `182736005..182736359` | `fcb695c8551a2b84336af2f788130a68d1c83c9783531a35fb57ee43be70aa60` |
| `Sn` result stripping/read-only comparison | `182735495..182735765` | `cd86a3640f57c7f3ea299598e2722b567205c08f60e6b3a0f183fba90091ad50` |
| `Re` Native user-message constructor | `191312309..191313825` | `643c2975cfe603827a217f3ebc358cdd155416c704bd0d6b37bcd0a25de1e33f` |
| `DVt` body (`function DVt`) | `189564393..189566538` | `2d239227114d8a8a6f9c14bce7f44192a2758fd8c3e8ef4226498d90f9eaf52a` |
| `DVt` full function, including `async ` prefix | `189564387..189566538` | `13894e860503c02698c05aa63d857eecb7ce51f0bb32486c59b10809761adf15` |
| `Iyo` result/context post-processing | `189566538..189566972` | `1cc8dbe7757f2f8d7b81b2270f56405ac0e9aca03a7f89e9fdee1d64f2d099b0` |
| `Acn` PostToolBatch hook generator | `189559207..189559539` | `63fff5397f8ce0b88ff0565cc8bb2aa7b086e4a150712bcd240370e5b9ed3bb2` |
| `Gm` API-created tool context | `206338016..206339787` | `8e026cb06fcaa95e1583bfe52e7c36f266d1d10c3e21fa8e017c940bb90bcb48` |
| `Yp` active session API object | `206482876..206484715` | `746fbe3ec8ac6ca58c088ee36aab0115ebb2e488b6b5f80e8f8c7517fd9cac0e` |
| `_S` result permission-denial recorder | `206295756..206296031` | `10e5e4d303fb7511341e7accbde841f2b9fe502965e07ea3510191606431d4de` |

The original `DVt` pin begins at `function DVt`. The VM probe evaluates the
same function with the preceding six-byte `async ` prefix included.

## Public input, advertised catalog, and preflight order

The public `$.tool.call` API object is created by `nw`. Its `call` method first
requires an object with a nonempty string `tool`; otherwise it rejects with
`<plugin>: $.tool.call takes the event's input: { tool, ...args }` (or the
object-shape error). Only after that does it dispatch to `eU`.

`eU` first extracts the input via `_se`, then checks that a supplied `consent`
is a string. That check happens before looking up the active session or its
tool list. A bad value rejects with
`<plugin>: $.tool.call: consent, when given, is a string`; a valid string is
not yet treated as permission and does not skip the normal tool check.

`Hu` returns the active plugin session API. Its `Yp` implementation sets
`tools: Wi`, and `Wi` calls the active resolved-catalog builder `di(w())`.
`di` composes the current tool-permission context with registered host tools,
MCP tools/clients, external tool sources, and other active catalogs, applies
its runtime filtering and final permission-context filter, and may incorporate
the selected agent's resolved tools or structured-output tool. The probe did
not execute this stateful catalog builder; it injected a controlled list at
`Hu().tools()` so the exact `eU` catalog consumer could be exercised without
claiming a full Native session.

This `B.tools()` value is an array of executable Native tool descriptors, not
the model-facing JSON tool-schema payload. `eU` resolves one descriptor `V`
from that list, then passes the virtual tool-use and `B.canUseTool` into `W1`.
The pinned eU path does not consume a ToolSearch wire-definition list. This is
a source-level separation: the probe stubs `Hu().tools()` and does not execute
the session catalog builder or the model request's deferred-schema assembly.
Therefore the evidence identifies the actual current session's resolved,
permission-filtered executable pool, but does not prove it is identical to the
subset of schemas currently present in a model request.

Before resolving a name, `eU` filters the returned list with Native `Mx`.
`Mx` excludes a wrapped V1 batch tool only when all four structural facts are
present: string `underlyingV1ToolName`, string `entryFieldName`, and function
`perEntryHookInputs` and `reassemble`. Then `yn` resolves the requested string
against each remaining tool's exact `name` or an exact, case-sensitive
`aliases` entry. Its catalog map preserves the first tool for a duplicate
name/alias. The Projects gate therefore sees the resolved tool's canonical
`V.name`, even when the request used an alias.

`Uoe` is false for all names except exact `WebFetch` and `WebSearch`. For those
names it also requires a Projects signal from the active feature flag, the
Projects-session environment flag, or session settings. A payload-sourced
permission permit with no user decision is an exception that makes `Uoe`
false. The preflight probe sets the actual Native feature-signal stub to true
for its WebFetch case; the existing wrapper oracle separately exercises the
ordinary-session, Projects-environment, Projects-session, and payload-permit
branches.

The observable error order is:

1. The public `nw.call` tool-input shape check runs before `eU`.
2. A non-string `consent` is rejected inside `eU` before session/catalog
   lookup.
3. A valid input is resolved against the current `B.tools()` list after the
   `Mx` filter. An unknown name is rejected before the Projects consent
   predicate; its error is
   `<plugin>: $.tool.call: no tool named "<name>" in this session`.
   The preceding log line lists only the filtered catalog's tool names.
4. Only a resolved tool reaches `Uoe(V.name)`. If the Projects predicate
   rejects its consent, `eU` reports the canonical name with
   `<plugin>: $.tool.call: <canonical-name> does not accept \`consent\` in a
   Projects session. Call it without \`consent\`, and the normal permission
   check will decide.`
5. Only after all these checks does `eU` construct its abort controller and
   tool context, append the consent as a human-origin message, mint a virtual
   tool-use row, and call `W1`.

The `mods_tool_call_preflight_289_probe.mjs` executes the pinned `nw`, `eU`,
`Mx`, `Ht`/`yn`, and `Uoe` source. It tests invalid public tool shape, malformed
consent, an unknown name in Projects mode, exclusion of a wrapped batch-tool
alias, and a Projects-restricted WebFetch requested through its alias. It
asserts the exact error order, canonical error name, filtered diagnostic
names, and that the Projects rejection happens before constructing the abort
controller or entering `W1`.

The same probe also executes exact `$Vt`/`I9`, `av`, and `hfo` source. With a
pre-aborted signal supplied to `I9`, invalid consent, an unknown tool, and the
Projects consent rejection all win before eU creates its per-call controller,
mints the virtual tool-use ID, or enters `W1`. After those checks pass, eU
creates its controller and `av` immediately aborts it from the caller signal;
eU still constructs the virtual row and enters the controlled `W1` stub with
the canonical tool name and an aborted context signal. Since the stub yields
no result, eU reports its exact `(aborted)` no-result suffix. This verifies
preflight and signal ordering, not cancellation behavior inside a real tool.

`nw.call` takes one formal input argument and dispatches only
`("tool.call", input)`. A second JavaScript argument such as
`{ signal: abortedSignal }` is ignored. The signal reaches eU from the hook
context: `jyo` calls `$Ot({ signal: s.abortController.signal, origin:
s.hookOrigin, ... })`; `$Ot` is `I9`, which preserves the supplied signal, and
`ya` forwards it to `qT` and the handler chain. If `I9` receives no signal, it
creates its own fresh `AbortController` signal. The probe calls the exact `nw`
and `I9` source with the second-argument signal pre-aborted, while the `I9`
default signal is live; the W1 stub observes the live signal. This separates
the actual hook cancellation channel from an unsupported call-options shape.

The request alias remains in the input checked by `nw` and resolved by eU.
After resolution, eU passes `V.name` to W1. In the production W1/jyo path,
`jyo` creates the inner `tool.call` payload from the current tool's canonical
`e.name`, so the downstream tool-call middleware sees the canonical name,
not the caller's alias. The probe dynamically verifies canonicalization at
the W1 boundary; the jyo hook payload is source-reviewed, not dynamically
executed.

The fixture stubs `_se` as identity; `Hu().tools()` as a fixed four-tool
catalog; `Ny`, the generic object check `N`, logging, host dispatch, the
`toolContext` projection, and the downstream `W1` path. It executes Native
`Mx`, `Ht`/`yn`, `Uoe`, `nw.call`, `I9`, `av`, `hfo`, and `eU` literally.
`Uoe`'s feature/settings helpers are controlled input stubs; the WebFetch case
sets its native Projects-session signal true. The dynamic catalog builder
`di`, active session accessor `px`, actual `jyo`/`ya`/`qT`/`eg` integration,
and actual tool execution are source-reviewed, not VM-executed in this probe.

## Call path, frame consumption, and visible output

`W1` resolves and runs the actual session tool. It delegates the tool call to
`jyo`, which wraps the core `zyo` run in `Myo`, configures `$Ot` with a local
core/managed `tool.call` handler, then enqueues the messages returned by `Iyo`.
`Myo` uses `MUn` and `XIe` to track each core `next()` run. The execution and
permission checks occur below the Mod result wrapper; choosing a different
result later cannot undo them.

`eU` creates a fresh virtual assistant tool-use row and consumes the `W1`
iterator itself. Its `bfo` extractor ignores any event with a `type` field and
accepts only a user message. It reads the matching `tool_result`, the first
`toolUseResult`, `is_error`, and `toolDenialKind`; it then resolves with only
the public Promise result/text/error shape or `{deny: ...}`. It does not forward
the inner frame to the normal turn emitter, transcript writer, SDK message
callback, or `setMessages`. Progress and non-user/event frames are discarded.
Other row fields and separate frame attachments, including `newMessages`,
`contextLayers`, and host metadata, do not become additional Promise fields.

This is the important distinction for Harness: the virtual row is a transport
inside the Native call chain, while the tool execution and its external effects
are real. Do not persist a second outer assistant tool-use/result pair merely
to reuse the ordinary visible-response path. Do preserve physical effects that
already happened, and settle only the result metadata selected by Native.

`Acn` is a separate PostToolBatch hook generator. There is no call to it in the
`W1` → `jyo` → `Myo`/`DVt` → `Iyo` path. Do not synthesize a PostToolBatch event
for each `$.tool.call()` solely because ordinary assistant tool calls have an
outer batch finalizer.

`Iyo` handles the Mod middleware's returned `context` after `DVt`. When that
context is accepted, the managed path runs the PostToolUse context processing
through `n2o`; the unmanaged path creates a `hook_additional_context` row via
`kcn`. This is a middleware `context` result, not the inner tool's
`context_modifier` or a `contextLayers` field from its execution frame.

## `next()` repetition, `ref`, replacement, and deny

`XIe.keep` records each run and returns its one-based index. `MUn` stores the
run's complete messages and raw result under that reference. `DVt` uses the
middleware's `ref` to recover a run, then `vUn` decides whether the middleware
passed that run through unchanged.

I evaluated the exact extracted `XIe`, `MUn`, `vUn`, `RUn`/`ilt`, and `DVt`
functions in a Node VM. The underlying core `run` callback was injected and
recorded an irreversible marker each time; no Native `W1`, tool implementation,
permission UI, MCP client, or process was executed. Calling core twice produced
refs 1 and 2 and two effect markers in each case:

| Final middleware outcome | Emitted message | Selected metadata |
| --- | --- | --- |
| Ref 1, same result | Run 1's original row | Run 1's row |
| Ref 1, replacement result | Synthetic replacement row | `RUn`/`ilt` copy Run 1's `mcpMeta` and `toolEndsTurn` |
| Ref 1, `deny` | Synthetic `permission-rule` denial row | Run metadata is not copied into the denial row |

The fake runner's effect markers remained `[run 1, run 2]` in all three cases.
This confirms that a `ref` selects output; it is not a rollback token. A
ref-backed rewrite must preserve the selected row's `mcpMeta` and
`toolEndsTurn`, as Native does. A deny follows a different `DVt` branch and
does not carry those selected-row fields. Managed PostToolUse behavior on deny
is conditional on `managedRan` and was not exercised by this focused probe.

`XIe.of(ref)` is a direct `runs[ref - 1]` lookup. For an ordinary tool,
`ref: 0`, an out-of-range reference, or an unknown reference returns no run;
`DVt` does not reject the reference. With no `deny`, it follows the synthetic
replacement branch using the middleware result and has no selected row from
which to copy metadata. A present `deny` takes the denial branch before that
replacement branch, even if a ref was supplied. The special account-memory
tool path can fall back to the last run and has a separate no-run refusal; it
is not the ordinary tool contract.

The same focused VM directly probed numeric, string, boolean, array, object,
null, and missing `ref` values. Since the Native lookup is literally
`runs[ref - 1]`, JavaScript numeric coercion applies: `true`, `[1]`, `"1"`
select run 1; `[2]`, `"2"` select run 2; `false`, `[]`, `{}`, `null`, `0`,
out-of-range values, `1.5`, and an unparseable string select no run. With no
selected run, even a result deep-equal to an already executed run still
produces a synthetic replacement row. A present `deny` still takes precedence
and produces the synthetic denial row.

That denial row has a separate exact Native shape. `DVt` calls `RCe` with the
virtual tool-use ID and `sourceToolAssistantUUID`, then `wUn` creates one
`tool_result` block whose `content` is the literal
`<tool_use_error>${reason}</tool_use_error>`, `is_error` is `true`, and
`tool_use_id` is the virtual ID. The outer row is a `type: "user"` message with
`toolUseResult: "Error: ${reason}"` and `toolDenialKind: "permission-rule"`,
plus the source assistant UUID and Native UUID/timestamp from `Re`. It has no
outer `isError`, `mcpMeta`, or `toolEndsTurn`: those fields are not supplied to
`Re`. This branch does not call the tool's result mapper. `eU` consumes the
denial marker and returns the API-level `{deny: ...}` outcome. If a managed run
already occurred, `DVt` may append its PostToolUse messages after this denial
row; they are separate output.

The public middleware site validator is `ma.check` (pinned above). It does not
validate `ref`'s type. Without a deny, it requires an own `result` property and
then validates the middleware context; the `Sn` helper uses strict equality
only when deciding whether to preserve `isReadOnly`, and does not reject a
malformed ref. Thus a public answer with `{ref: 0}` but no `result` is rejected
before `DVt`, while an object that has an own `result` property can reach the
internal coercive lookup. JSON middleware cannot express JavaScript
`undefined` as a value, but may express `null`; an in-process handler could
also supply an own `result: undefined` property. The internal
`DVt({ref: 0, result: ...})` no-selected-run replacement behavior is therefore
confirmed, while a no-result synthetic `DVt` row is not normally reachable
without a deny. The controlled VM did not execute Native's tool-specific
`mapToolResultToToolResultBlockParam` used to construct its content, so it does
not establish the exact content payload of that raw helper-only case.
A public JSON answer should use `{ref: 0, result: null}` to represent an
explicit null result; the current Rust dispatcher observes the property
through `Value::get`, preserving `Some(&Value::Null)` versus an absent key. Do
not use an absent `result` to test the internal no-run replacement branch,
because the Native site validator rejects that public shape first.

The `ma.check` and `Sn` source ranges come from the helper chunk beginning at
Native byte `182637043`, SHA-256
`b8b83549f7b4273ed3f6b4669d666e535f9abfd831ea3952e9387ac73500e85c`. Their
runtime checks were executed with the exact extracted functions, alongside
`XIe`/`DVt`, and controlled input values; no Native tool implementation was
run.

The probe used these hash-checked Native slices: `XIe`
`182763200..182763365` (`bcb84d348c2772be59766f01ba7d207eca4e1dc1da9ecdec630aa5268f979294`),
`vUn` `187737832..187737934`
(`31df52258cb1479b8ae29ca06ab2990da7383ced46550b116e37c30d3d53dc60`),
`MUn` `187738611..187739221`
(`ac07c963162f666d98765743c3183b45e8ed3086d83e1f73745f7c000f33d47e`),
and the full `DVt` range above. `run`, tool result decoding, and imported
environment helpers were controlled test inputs; this is not a Native tool
integration test.

## Permission-denial result accounting

The normal query has a turn-local `_S` wrapper. It owns the `permissionDenials`
array and records when its wrapped `canUseTool` result satisfies Native's
`a6e` predicate. That array is later placed on the SDK `result` object. The
query's `ToolUseContext` also gets the per-query recorder `xi` as
`onPermissionDenial`.

The API path uses a different context. `Yp` supplies `toolContext` by calling
`Gm(pu(mu()), ...)`; `Gm` explicitly builds the context with
`onPermissionDenial: () => {}`. `eU` passes this context to `W1`. In `zyo`, a
permission result other than allow calls only the optional
`s.onPermissionDenial?.(tool, toolUseId, input)` callback. For `$.tool.call()`
that callback is the no-op. The produced frame can still carry
`toolDenialKind`, which `eU` consumes to return `{deny: ...}`, but that marker
does not append a `permission_denials` entry.

Therefore the Native `$.tool.call()` virtual denial does **not** update the
current turn's `result.permission_denials` through this path. Harness should
not call its result-side `record_permission_denial` just because a virtual
frame has `toolDenialKind`; the ordinary main-query tool path has its own
non-no-op `xi` recorder and remains separate.

## Agent-specific API branch

`eU` reads the first `toolUseResult` from its virtual user frame through `Aq`,
which clones the raw value. `_fo` recognizes an Agent launch only when the
selected tool is Native's Agent tool and that raw result has
`status: "async_launched"` plus a string `agentId`. The returned Agent API
result is rebuilt by `LAt`: `{agentId}` plus `resolvedModel` only when `ptr`
finds a string at `rawResult.resolvedModel`. Other raw-result fields, including
`status`, are not copied into the API result.

`UOt` polls the registry every 150 ms. Its no-answer budget is ten minutes:
`VDe` is 10, `jOt` is 60, and the imported millisecond factor `kK` is 1000, so
`WOt = 600000` ms. It searches only `local_agent` rows for the requested
`agentId`. The Native terminal predicate is exactly `completed`, `failed`, or
`killed`; the status spelling `cancelled` is not terminal. For `completed`, it
returns `{text}` from the latest assistant transcript row whose text is
nonempty. `n5r` concatenates that row's text blocks with no separator and
`kfo` scans transcript rows backward. For `failed` or `killed`, it returns
`{text, isError: true}` with
`<plugin>: $.agent.spawn: the subagent <status>` and appends `: <value>` when
nonempty. `<value>` is the row's string `error` when present; otherwise it is
the transcript text. A string error (even empty) takes precedence over the
transcript.

If no matching Agent row has ever appeared, abort returns
`{text: "<plugin>: $.agent.spawn aborted", isError: true}` immediately, and
timeout returns `{text: "<plugin>: $.agent.spawn: no answer within 10 minutes",
isError: true}`. Neither has a settlement promise. If a row was already
observed, abort or timeout goes through `qOt`: it returns the same error text,
sets the row's `notified` field to `false`, and attaches `settled`, which is a
`zOt` promise waiting until the registry entry disappears or reaches a
terminal status. In `eU`, this `settled` promise is passed to `leftRunning`
before returning `FAt(undefined, text)`, whose shape is
`{result: undefined, text, isError: true}`. JSON serialization omits the
undefined `result` property. A row that disappears after first being observed
uses `GOt`: nonempty transcript text is treated as success; no text yields
`<plugin>: $.agent.spawn: the subagent's record was evicted before its answer
was read` with `isError: true`.

The `isLaunchOnly` path is supplied by `Yyo`, the `$.agent.spawn` wrapper; an
ordinary `$.tool.call()` does not set it. `eU` calls `BOt`, which replaces the
caller signal with a fresh non-aborted signal and sets the settle timeout to
infinity. It registers the settle promise via `leftRunning`, then immediately
returns `{result: LAt(agentId, resolvedModel), text: ""}`. The wrapper uses
that promise to release its launch slot after the Agent settles. The ordinary
settled path returns `{result: LAt(agentId, resolvedModel), text}` on success;
errors return the `FAt` shape above. This is distinct from an ordinary tool
result and must not be flattened into the normal `$.tool.call()` outcome.

`mods_tool_call_agent_289_probe.mjs` executes the pinned `eU`, `UOt`, terminal
predicate, result constructors, and transcript helpers with a fake `W1`, a
controlled registry, and a deterministic clock/cancellation seam. It covers
complete/failed/killed results, absent records and ten-minute timeout,
caller abort before/after the Agent row appears, a `cancelled` row, and record
eviction with and without transcript text. It does not start a Native Agent or
run a real tool.

## Trusted origin propagation

The actual `jyo` caller constructs `$Ot` with
`origin: s.hookOrigin`. `$Ot` is `e => I9(e)`, and `I9` copies `e.origin`
unchanged into the API caller. The `ya` dispatcher uses that origin to select
handlers and passes its normalized form into the handler chain. `eU` uses
`n.origin ?? [plugin]`; `$At` then assigns `hookOrigin` to that exact value
and keeps the current plugin separately as `hookCaller`. It does not append a
`{plugin, tier}` item. If the caller supplied no host chain, Native's fallback
is `[plugin]`.

## Verification boundary

The checked-in `mods_tool_call_289_oracle.mjs --check` passed for 2.1.289. It
executes literal `eU` and small helpers against a fake `W1`; it covers wrapper
success, logical error, deny, missing result, cancellation, and Projects
consent cases. The companion `mods_tool_call_ref_289_probe.mjs` executes
hash-pinned `ma.check`, `Sn`, `XIe`, `DVt`, `RCe`, `wUn`, and `Re` functions
with controlled run/tool inputs; it passed the public-validator, coercion,
pass-through, replacement, and denial assertions. `W1`/`zyo` tool execution
itself, real permission prompts, tool side effects, MCP effects, and a running
Native Agent were not dynamically executed. The companion
`mods_tool_call_agent_289_probe.mjs` passed its controlled registry/clock
assertions for Agent API result fields and lifecycle. The new
`mods_tool_call_preflight_289_probe.mjs` passed its tool-shape, consent,
filtered-catalog, alias, Projects ordering, caller-signal propagation, and
pre-abort cases with a controlled session catalog and W1 stub. The dynamic
`di` catalog builder and real tool execution remain source-reviewed only. No
production code or Cargo files were changed for this audit.
