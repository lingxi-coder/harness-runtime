# Native Desktop `Client` element and surface runtime in 2.1.289

This note records the source-observed Desktop `Client` contract from the pinned
Claude Code 2.1.289 macOS arm64 executable. It covers the parent hook tree, the
surface-module source/load path, the Node VM callback boundary, and output
identity. Current Harness/Product consumer implementation and open acceptance
gates are tracked in the [current integration gap record](mods-ui-client-2.1.289-gap.md).
This note does not claim full Desktop frame IPC byte capture or a Native CLI run.

Run the source-pinned Node VM probe with:

```sh
node scripts/tests/mods_ui_client_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

Check the SDK control request/result, surface-module manifest, JSX transform,
and terminal pointer/key producer pins with:

```sh
node scripts/tests/mods_ui_client_control_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The executable SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
Ranges are zero-based, half-open absolute byte offsets in that executable.

## Parent render tree and ownership

The hook JSX helper accepts `<Client module="./board.tsx" key="board" data={...}
width={...} height={...} flexGrow={...} />`. `module` and `key` must be
non-empty strings; `module` names a surface module relative to the hook source
file, and `key` is the Client's address used as `ui.message.element`. The JSX
`data` spelling is only an alias: the emitted parent tree stores it under
`props.props`. There is no Client `data` property. The emitted element has
`type: "Client"`, props `key`, `module`, optional plain-data `props`, optional
`width`, `height`, and `flexGrow`, no children, and an internal initial
`client: { plugin: "" }` stamp.

The tree validator allows exactly those six Client prop names. `key` and
`module` pass the common string validator; `props` passes the native plain-data
validator. `width` and `height` accept finite numbers from 0 through 10,000 or
a percentage string of one to three digits followed by `%`. `flexGrow` accepts
a finite number with absolute value at most 10,000. The Client is a leaf. It
must carry a client stamp from a plugin in the active render chain.

`ui.resolve` keeps the Client's owner identity through hook chaining. A Client
without an owner stamp is stamped with the current resolver plugin. A stamped
Client can be returned only if the chain already saw the same `(plugin, key,
module)` tuple; a render hook may rewrite props but cannot replace that tuple.
This is validation provenance. The internal stamp is not the child module's
runtime instance ID.

The plugin scanner collects Client module path literals into `scan.clients`.
The AST scanner finds Client helper calls. `So` requires the literal to be a
relative path, resolves it against the importing source file's directory,
rejects paths outside the plugin root, and normalizes the result to a
plugin-relative module key. The loader's `WEt` then resolves that normalized
key with `path.resolve(pluginRoot, ...module.split("/"))` and records
`{ module, modulePath, component, source, linked, links }` in the loaded
plugin's `surfaceModules`. `component` is the named export discovered from the
module's source: a default export wins; otherwise there must be exactly one
capitalized component export. The entry and its static imports are read from
the plugin file graph. The import resolver rejects imports that resolve
outside the plugin folder. These pinned functions accept relative filesystem
source paths; they do not load `file:` or network URLs. `surface:///...` is a
generated module-graph identifier for local files, not a remote fetch URL.

The authored `<Client module="./card.tsx">` spelling is not kept as the
runtime key. The `Client` constructor itself copies the string it receives,
but the Native hook-module loader calls `Uas(qEn(source), currentModulePath,
pluginRoot)` before creating each `SourceTextModule`. `Uas` visits parsed
`Client(...)` or `h(Client, ...)` calls and replaces the module literal with
`So({ spelled, file, root }).module`. The same `So` result feeds `Or`'s
`scan.clients` set. Thus `./card.tsx` in `/plugin/register.tsx` becomes
`card.tsx`, while the same literal in `/plugin/hooks/register.tsx` becomes
`hooks/card.tsx`.

Native prepares modules from those normalized scan keys: `WEt` resolves the
key under the plugin root and returns it unchanged as `surfaceModules[].module`,
`fHr` collects the entries, `qEt` attaches them to the loaded plugin, and the
manifest builder carries the same `module` field. The surface VM's loader then
uses exact `D.get(module)` lookup. This is compile-time source rewriting, not a
Host alias or a renderer-side path normalization.

The control probe directly executes the pinned `Client` constructor, `So`,
`Uas`, and `Or` functions for root and nested importers; its output shows the
tree keys match the normalized scan keys. The test injects only parser-returned
AST nodes for the rewrite call, so it does not execute Native's parser or the
full plugin source loader. The source pins and the actual loader call path are
also asserted.

## Surface module and JSON tree

The module is evaluated by the Native `surface` environment in a Node `vm`
context. The default top-level module-load budget is 10 seconds. The loader
selects `exports[component]`; that export must be a function with the contract
`(props, surface) => tree`. There is no default-export fallback. Each plugin
environment caches loaded modules by their `module` path and is keyed by the
plugin's current environment ID. A changed environment disposes the old
surface environment.

The host compiles non-`.js` hook source with the executable's built-in
`Bun.Transpiler`, using `{ loader, macro: false }`. `.tsx` and `.jsx` source is
prefixed with the exact pragmas `@jsxRuntime classic`, `@jsx h`, and
`@jsxFrag Fragment`; the `claude:surface-runtime` module exports those `h` and
`Fragment` bindings. `.js`, `.mjs`, and `.cjs` are passed through unchanged.
The loader map is `.ts`, `.mts`, and `.cts` to `ts`; `.tsx` to `tsx`; `.jsx` to
`jsx`; and `.js`, `.mjs`, and `.cjs` to `js`. `qEn` skips transpilation for
`js`, uses no pragma for `ts`, and prefixes the JSX pragma for `tsx` and `jsx`.
There is no Babel or automatic React JSX runtime in this path. Entry export
selection is also source-pinned: a `default` export wins; otherwise exactly
one capitalized component export is required.

The `surface` object contains `elements`, `state`, `columns`, `rows`,
`setState`, `every`, `onPointer`, `onKey`, and `post`. `surface.elements` has
exactly `Box`, `Text`, `Button`, `Input`, `Select`, `Link`, `Code`, and
`Markdown`. It deliberately excludes `Client`, `Raster`, `Image`, and engine
nodes. The helper constructors call the module's `h` function with a type,
props, and children.

The module result is read in the VM and copied into a fresh, bounded JSON tree.
The emitted shape for an element is:

```json
{
  "type": "Box",
  "props": { "key": "panel", "flexDirection": "column" },
  "children": [{ "type": "Text", "hover": { "color": "green" }, "children": ["text"] }]
}
```

`props` is optional plain JSON. `hover` is an optional flat object: `scope` and
the hover style fields are siblings, not a nested display record. The schema
allows an optional non-empty `scope` string up to 64 characters. The top-level
`scope` is a hover-group name, not a plugin name. A Box's `props.key` is the
default nearest keyed-Box scope; absent that scope, an explicit `hover.scope`
must be nonempty, at most 64 characters, and contain no control characters.
The top-level `group: { plugin }` stamp is separate ownership metadata. For
Box/Text the validator checks it when `hover.scope` is present; Button uses its
`press` stamp instead. The validator requires a nonempty owner plugin stamp
but does not compare the scope string with the plugin name. With no explicit
scope, a keyed Box ancestor is required; a hidden keyed Box without a hover
scope is rejected. Native advises keeping that keyed Box visible and placing
`display: "none"` plus `hover.display: "flex"` on a Box inside it. `children`, when
present, must be an array of strings or element nodes. Functions and
`undefined` values are omitted from copied data, non-finite numbers become
`null`, array `undefined` becomes `null`, sparse arrays are refused, and
`toJSON` is not consulted. `Button` must carry a function at `onPress`; `Input`
and `Select` must carry a function at `onEvent`. Those closures are retained
inside the surface environment and represented in the emitted tree by a
numeric `held` handle. The functions themselves do not cross the bridge. A
held handle is retired when its drawing is replaced or unmounted.

The classic JSX factory does not stamp `group`. The Native `ui.resolve`
transformer traverses the returned parent tree and handles Box/Text nodes; it
adds `{ plugin: currentPlugin }` only when `hover.scope` is present and the
group is absent or already belongs to the current plugin. A Box/Text carrying
another plugin's group is preserved only when the current hook received that
`(plugin, scope)` capability through `next(e)`; otherwise the resolver refuses
the cross-plugin group. Button is handled in the press-stamped path and does
not receive a `group`; its `press` stamp owns an explicit hover scope.

The outer UI validator then applies the element-specific Native props rules.
All props must be plain data with no unlisted keys. `Box` allows `flexDirection`,
`flexGrow`, `flexShrink`, `flexWrap`, `alignItems`, `alignSelf`,
`justifyContent`, `gap`, `columnGap`, `rowGap`, `width`, `height`, `minWidth`,
`minHeight`, `margin` and its `X`/`Y`/side forms, `padding` and its `X`/`Y`/side
forms, `borderStyle`, `borderColor`, `borderDimColor`, `backgroundColor`,
`overflow`, `display`, `position`, and `top`/`left`/`right`/`bottom`; `key` is
handled separately as its hover address. `Text` allows `color`,
`backgroundColor`, `dimColor`, `bold`, `italic`, `underline`,
`strikethrough`, `inverse`, and `wrap`.

Hover style keys are validated separately from base props. Box accepts
`borderStyle`, `borderColor`, `borderDimColor`, `backgroundColor`, `display`,
`top`, `left`, `right`, and `bottom`; Text and Button accept `color`,
`backgroundColor`, `dimColor`, `bold`, `italic`, `underline`,
`strikethrough`, and `inverse`. Hover color strings match
`^[#a-zA-Z0-9_().,% -]{1,40}$`; boolean fields require booleans; offsets are
integer character-cell values with absolute value at most 10,000; enum values
use the same Native sets as base props. Hover `display` is only `flex` on a Box
whose drawn `display` is `none`; hover `borderStyle` requires a drawn base
border, and hover offsets require `position: "absolute"`.
The broad tree schema has a `hover` slot on its `div`/`span`/`b` branch, but
the second-stage Native validator rejects hover on those node types; only
Box/Text/Button accept it.

Style values use Native `Y6t`: `flexDirection` is `row`, `column`, `row-reverse`,
or `column-reverse`; `flexWrap` is `nowrap`, `wrap`, or `wrap-reverse`;
`alignItems` is `flex-start`, `center`, `flex-end`, or `stretch`; `alignSelf`
is `flex-start`, `center`, `flex-end`, or `auto`; `justifyContent` is
`flex-start`, `center`, `flex-end`, `space-between`, `space-around`, or
`space-evenly`; `overflow` is `visible|hidden`; `display` is `flex|none`;
`position` is `relative|absolute`; `wrap` is `wrap`, `end`, `middle`,
`truncate-end`, `truncate`, `truncate-middle`, or `truncate-start`. `borderStyle`
accepts `single`, `double`, `round`, `bold`, `singleDouble`, `doubleSingle`,
`classic`, `arrow`, `dashed`, or `quote`. Width/height/minimum dimensions are
finite numbers from 0 through 10,000 or strings matching `^\\d{1,3}%$`;
flex, gap, margin, and padding numbers are finite with absolute value at most
10,000; offsets are integer character-cell values with absolute value at most
10,000. Colors are strings of 1–40 allowed color characters, and the boolean
style fields accept only booleans.

`Button` requires `key` and `label`, with optional `hotkey`, `action`,
`plain:true`, `dimColor`, `variant:primary|secondary`, `role:dismiss`, and
`autoFocus:true`. `Input` requires `key` and optionally accepts `label`,
`placeholder`, `value`, `submitLabel`, and `autoFocus:true`. `Select` requires
`key` plus 1–64 options shaped `{ value: string, label?: string }`; values must
be unique. It optionally accepts `label`, selected `value`, and `autoFocus:true`.
`Link` requires a non-blank string `href` and optionally accepts a non-blank
string `label`; its href is limited to 2,048 characters as written and the label
to 10,000. For non-terminal remote surfaces, a Link remains clickable only
when its href is printable ASCII, a parseable canonical HTTPS URL or localhost
HTTP URL, contains no credentials or raw `@`, and stays within the encoded
length bound. Other hrefs are rendered as plain text rather than rejecting the
whole tree.
`Code` requires `source` and optionally accepts `language`, `path`,
`startLine` (integer 1–1,000,000,000), `format:source|diff`, and
`wrap:wrap|truncate-end`. `Markdown` requires `text` and optionally accepts
`key`, `dimColor`, and at most 256 `pressableLinks`, each a non-empty href of at
most 2,048 characters. All leaves reject children. The embedded runtime refuses
nested `Client`, `Raster`, `Image`, and `engine` nodes. A surface module's
Markdown cannot supply `onLinkPress`; it handles its own pointer through
`surface.onPointer`.

The runtime bounds trees to 20,000 nodes, depth 32, 100,000 serialized
characters, 20,000 values per props/hover payload, and data nesting depth 32.
The same plain-data rules apply to `surface.post(data)`. The serialized render
tree and posted `data` use separate channels and different envelopes.

## Load, host callbacks, and identity

The Native host constructs the surface VM with host callbacks for `schedule`,
`post`, `startTimer`, `stopTimer`, and `fault`. Its mounted instance exposes
`render`, `setProps`, `resize`, `pointer`, `key`, `runHeld`, `dropHeld`,
`hasPointerListener`, `hasKeyListener`, and `unmount`. A render stages the
instance's `render` callback, executes `__surface__.run()` in the VM, and
returns the JSON tree string. `resize` updates `surface.columns` and
`surface.rows`; `setState` invokes `schedule` once until the next render;
`every` uses the host timer callbacks; `post` sends a serialized data payload.

`pointer(payload)` and `key(payload)` pass their payloads unchanged to the
registered surface listener. `runHeld(handle, event)` looks up the retained
closure and passes it `event`. This VM/runtime source does not define a fixed
DOM-style event shape for those values, so their external input schema remains
unresolved here. The parent hook events `ui.press`, `ui.input`, and `ui.select`
are separate dispatch contracts and do not by themselves establish the
payload sent to the surface module's local callback.

`surface.post(data)` goes through the host's `post` callback and is bound by the
Native Desktop wrapper to this `ui.message` request:

```json
{
  "plugin": "plugin-name",
  "message": {
    "surface": "desktop",
    "component": "parent-render-component",
    "requestId": "parent-ui-render-request-id",
    "element": "client-key",
    "module": "./board.tsx",
    "data": { "kind": "example" }
  }
}
```

`ui.message` is not the render-tree format. A response with `props` can update
the live Client props if its mount generation still matches. The posted
message's `requestId` is the parent `ui.render` request ID, not the derived
Client drawing ID.

There are three identities in the source. The parent site is `(surface,
component, parentRequestId)`. The child drawing request ID is
`parentRequestId + NUL + "client" + NUL + key`. The host's reusable Client
instance key is `(plugin, surface, parentRequestId, key)`; the module name is
checked separately when deciding whether that instance can be reused. The
surface VM has a further environment-local numeric instance ID used by its
callbacks. The Client module manifest is content-addressed: it hashes the
plugin's linked source files, module entries/components, runtime identifier,
and limits. A parent `ui.render` result carries the `tree` and a
`clientModules` map from plugin name to that manifest hash; `clientModule`
returns the manifest and local source files.

Failure phase follows the lifecycle boundary: environment/module/export/mount
errors are reported as `load`; render calls or returned invalid trees as
`render`; rejected callback promises or host-dispatched runtime failures as
`run`. These reports use the parent render component/request ID with the
Client key and module. The companion
[ui fault evidence](mods-ui-fault-2.1.289.json) pins the internal request
schema, hook fields, current-Client gate, and fault producer.

The manager's `S` fault transition is terminal for that mounted surface: it
calls `v`, which invalidates the mount generation, releases held callbacks,
clears scheduled render timers, unmounts the VM surface (whose `Go` instance
unmount stops its per-VM timers), sets `surface` to undefined, then publishes a
`{ status: "failed", text }` snapshot and raises the fault. Internal run
callbacks use `y`/`S`; a module render failure uses `M`/`S`; load failures use
`ue`/`S`. Afterward, `schedule` is a no-op without a surface, `resize` records
new dimensions but invokes `surface.resize` and `M` only when a surface exists,
and `setProps` reaches an `M` that returns without a surface. These paths do
not produce a later successful frame from the same failed VM instance. An
explicit `remountPlugin` or disposed environment resets the instance to
loading and calls the mount path again.

There is a narrower recovery path for an exception in the outer React rendering
of a Client tree. The terminal wrapper passes `i.getSnapshot()` as the React
error boundary's `resetKey` and calls `i.threw` on boundary errors. `threw`
records `thrownAt`; `drawnAgain` remounts only if that marker exists, the
current fingerprint differs, and the instance still has holders. The
fingerprint combines the prior/current terminal geometry (`columns`, `rows`,
`conversationColumns`) and serialized handed props. It does not compare a
parent revision token: unchanged props and geometry do not retry, while changed
props or geometry can pass the gate without a distinct revision field. A
runtime `run`/`load`/manager-render fault does not set `thrownAt`, so that
`drawnAgain` path does not recover it. The exact `BO` error boundary itself
clears `hasError` when the snapshot object identity changes, independent of
any revision label; the Client subtree still renders `snapshot.text` while
status is `failed`.

The pinned concrete React wrapper calls `acquire` with `surface: "terminal"`.
The manager is shared and accepts other surfaces, but this captured wrapper is
not evidence for an external Desktop DOM consumer's fallback/latch policy.
No such Desktop consumer is present in the inspected Native source, so whether
an external same-revision DOM frame clears its own fault leaf remains
unverified.

The following fault-registry discrepancies are historical audit findings and
are superseded by the [current Host implementation](../../crates/hooks/src/mod_ui_fault.rs).
`ClientUiRenderSite` now
keys duplicate suppression by `{surface, component, requestId}` rather than
plugin; `dispatch_client_ui_fault` selects the lazy `bornAt` version after the
async hook/listener checks; and fault-run eviction clears the corresponding
drawn-site failure state. The current code and acceptance boundary are tracked
in the [integration gap record](mods-ui-client-2.1.289-gap.md) and the
[working-tree integration validation receipt](claude-mods-integration-validation-2026-10-03.json).
This source review did not run the Rust tests or full integration gates, so it
does not claim those checks passed.

The pinned Native facts remain: `MD(site)` keys `surface`, `component`, and
`requestId`; a repeat is suppressed only when both the original `bornAt` and
current site version are recorded; and Native's `BC(site)` eviction clears
cached site version state when no mounted client or reader remains. The Rust
reason normalizer matches the pinned 500-unit pre-truncation, one plugin and
one Client prefix removal, empty fallback, separator replacement, and safe
200-unit final truncation. Native also scrubs lone UTF-16 surrogates; JSON
decoding into Rust `String` cannot represent an escaped lone surrogate, so that
wire-decoding edge remains an open boundary.

The current Harness Host and Product Electron Client renderer/bridge exist;
their source and remaining acceptance gates are in the linked integration gap
record. This Native-source note still cannot prove exact external Desktop frame
IPC parity: the concrete React consumer found in the pinned executable is the
terminal-only wrapper described below.

## Bundled Bun build and hooks-types asset

The pinned executable embeds Bun `v1.4.3`, build `eecfd55de`, commit
`eecfd55debb281c3fe3518098b1a49f76429637c`. These independent strings in the
same pinned binary identify the version and build:

| Embedded string | Byte range | SHA-256 of string bytes |
| --- | ---: | --- |
| `https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-darwin-aarch64.zip` | `59,270,064..59,270,146` | `54bca5df77e1bae43374d029a0b3f74b1f66d86b1210ac438a25b5820426d96c` |
| `bun-v1.4.3` | `59,383,288..59,383,298` | `0586e0346f58e216ca5b4e3047aae76faaea1b57744d8d340238add0d24193e1` |
| `eecfd55debb281c3fe3518098b1a49f76429637c` | `59,383,304..59,383,344` | `9169a3db8abbb001ae011addf9a0b416da0071465df6bc85504b0f10d5a8387d` |
| `Bun v1.4.3 (macOS arm64)` | `59,556,520..59,556,544` | `e8449c6f3933199578edecc8ab92efb35cf41bee37010bfc528871af72106cc9` |
| `1.4.3+eecfd55de` | `59,541,632..59,541,647` | `0b192641389c30267119a410b8c89e0d265b74724bf8be0855ac5505f3a7b991` |
| `Bun v1.4.3 (eecfd55de) macOS Silicon` | `60,558,496..60,558,532` | `bcceebebf712d8df0362af1138d3e66396bb9c165c77cc5951eee1baf787264f` |

The executable's `--version` and `BUN_BE_BUN=1 --help` still select the Claude
CLI; they do not expose the Bun command line. The installed Bun is 1.3.11, so
the actual Native 1.4.3 TSX transform has not been run for a byte-for-byte
golden.

The `__BUN` Mach-O segment at file offset `68,206,592` contains a standalone
module graph with a 160,738,634-byte payload and the public Bun graph trailer.
The graph's 32-byte offsets record points to 2,414 fixed 52-byte module
records. The `claude:hooks-types` path
`/$bunfs/root/state-library.js-wy8y4843.txt` is record 2,184; its Latin-1
contents span 4,060 ASCII bytes at graph offset `152,588,771`, SHA-256
`7908c3310f4764acd78ef09276fc3ed5c1315e456b5aa95aa18363b8baf87b55`. Executing
only that extracted source in an isolated Node `SourceTextModule` confirms its
runtime exports are `atom`, `derive`, `memberOf`, `read`, and `update`. The
probe writes no asset file and prints only the selected record metadata, hash,
and export names. It uses Bun's public
[standalone graph implementation](https://github.com/oven-sh/bun/blob/main/src/standalone_graph/StandaloneModuleGraph.rs)
for the record layout and validates every selected pointer against the pinned
binary's graph bounds.

The asset probe also executes those five exports directly from the extracted
source in a controlled `SourceTextModule` VM. `atom(ref, initial, options?)`
returns a frozen object with only `ref`, `initial`, and optional `shape`; its
ref is a frozen `{ plugin, key, id? }` copy, extra ref keys are discarded, and
the initial value is recursively frozen in place. For an unshaped atom, a
stored `undefined` reads as `initial`; for a shaped atom, only a stored
`{ shape: atom.shape, value }` with matching shape and non-`undefined` value is
used. A mismatch falls back to `initial`.

`derive(sources, compute)` requires an array, freezes the derived object and
source list, preserves atom/derived sources, and converts raw refs to frozen
`{ plugin, key, id? }` copies. `read($, source)` accepts an atom, a derived
value, or a raw ref and reads through `$.state.get`. Derived reads gather
versions in source order, flatten nested version arrays, and memoize the
compute result by `versions.join(",")`; unchanged versions reuse it and a
version change recomputes. The compute function receives source values in the
declared order.

`memberOf(family, event)` scopes an atom or raw ref to `event.requestId`; if
that is undefined, the existing family/ref ID remains. The returned atom/ref
is frozen and extra raw-ref fields are omitted. `update($, target, change)`
accepts an atom or raw ref and performs optimistic compare-and-set through
`$.state.get`/`set`. It passes `{ ifVersion: held.version }`, re-runs `change`
after a failed set, returns the unwrapped next value on success, and shape-wraps
stored atom values when needed. After 64 failed writes it throws exactly
`update: the value was written by another every time it was read, up to the
bound on tries; nothing was written`. A missing version is still passed as an
own `ifVersion: undefined` property. The controlled differential fixture
covers a shaped mismatch/read/update, versionless read/update, derive memo hit
and version bump, `memberOf` with and without a request ID, conflict retry, and
exhaustion. It records only fixture outputs and asset identity; it does not
copy the bundled source into the repository. The serialized observable
contract fixture SHA-256 is
`bf6c66bf48aac059cee43b106610edae0439aebd970de1442fac651557dc938c`.

Run it with:

```sh
node --experimental-vm-modules scripts/tests/mods_ui_client_asset_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

## Native Desktop control requests and results

These schemas are the SDK host's correlated control request bodies. The
transport envelope supplies `request_id`; the matching handler replies through
the same request object, so `request_id` is not duplicated inside the bodies
below. `ui_invalidate` is different: it is an asynchronous system event.

`ui_render` accepts `{ subtype: "ui_render", surface, component, instance_id,
props }` and these optional fields:

- `client_id`: 1–64 characters matching `[A-Za-z0-9._-]`.
- `viewport`: positive integer `columns` and `rows`, plus optional boolean
  `isFullscreen`.
- `on_screen`: omitted, `null`, or `{ first, last, of }`, where the first two
  are non-negative integers, `of` is positive, and `first <= last < of`.
- `content_rows`: non-negative integer; `keyed`: up to 512 layout rows, each
  `{ plugin, key, top, bottom }` with string plugin/key and non-negative
  integer row offsets.
- `bench`: `{ seq: integer, t0: number }`, echoed unchanged.

`surface` is `desktop`, `mobile`, or `vscode`; `component` is one of
`AskUserQuestion`, `UserMessage`, `AssistantMessage`, `ToolUse`, `ToolResult`,
`ToolGroup`, `ToolProgress`, `CommandOutput`, `Spinner`, `TurnDuration`,
`InfoNotice`, `SessionMode`, `PromptHint`, `AbovePrompt`, or `Pane`. The
required `props` is a string-keyed record of plain JSON values. The result is
`{ tree, props, rewritten, hooked, client_modules?, bench? }`: the first four
fields are always present, `client_modules` is emitted only when non-empty,
and `bench` is present only when the request carried it.

These request and nested object schemas use Native's ordinary Zod object
factory, whose unknown-key mode is `strip`; handlers consume
`safeParse(...).data`. Unknown keys at the request, viewport, `on_screen`,
keyed-row, `bench`, and each `ui_client_press.event` object levels are dropped
before dispatch. This does not strip keys inside `props` or `data`, which are
arbitrary JSON values.

When `client_modules` names a plugin hash, the renderer requests
`{ subtype: "ui_client_module", plugin }`. The result is the manifest itself:
`{ plugin, hash, modules, runtime, limits, files }`. Each module has
`{ module, entry, component }`; `limits` has integer `nodes`, `depth`, `chars`,
`values`, and `dataDepth`; each file has `{ key, source }`. `files` contains the
runtime and hooks-type modules first, then the normalized module entries and
their linked files in the host's graph order, with duplicate keys omitted.
Each `source` is compiled JavaScript with relative imports rewritten to the
manifest's import-map keys. The `modules` array preserves the sorted normalized
module order; `entry` is a `surface:///` import-map key and `component` is the
selected export name. The hash is SHA-256 over the Native serializer applied
to the object `{ files, modules, limits }`. Its imported `S` helper calls
`JSON.stringify(e, n, r)`, and this call supplies only `e`, so the preimage is
ordinary JavaScript `JSON.stringify` with no replacer or spacing. It preserves
the actual insertion order of object keys; it does not sort keys into a
canonical order. Unicode remains literal in the JSON string and contributes
its UTF-8 bytes to SHA-256. The controlled probe fixture
`{ files, modules, limits }` with Unicode source hashes to
`6c25f99202804ec19895774f28680df330bc61a662c6d7eb4cf03802e49f195e`; this is
a fixture golden, not a claimed live plugin manifest hash. The outer `plugin`
and `hash` fields are not part of that preimage.

The manifest's `claude:surface-runtime` file is generated from Native template
`wO`: it embeds the classic JSX factory string `Ger`, exports its `h` and
`Fragment`, and embeds `install` from `g8n`. Evaluating those exact source
strings and the `wO` template yields a 31,103-character runtime file whose
UTF-8 SHA-256 is
`833ab9dd15983d1fbdad634bf41a7ea633f092ac806539c7e927100f0e89e1b7`. The
second builtin is `claude:hooks-types`, bound as `Etr` to the BunFS asset
`/$bunfs/root/state-library.js-wy8y4843.txt`; its graph record, content range,
digest, and runtime exports are pinned above.

The address shared by `ui_message`, `ui_client_press`, and `ui_client_fault`
is `{ plugin, component, instance_id, client, module }`. All are strings; the
four non-component address strings have a 256-character maximum, while
`component` is the render-site enum above. The request-specific fields and
results are:

| Request body | Extra request fields | Result body |
| --- | --- | --- |
| `ui_message` | `data`: plain JSON; the Client post limit is 2,000 values, depth 32, and 100,000 serialized characters. | `{ handled: boolean, props?: plain JSON }` |
| `ui_client_press` | `element` string (max 256); `event` is `{type:"press"}`, `{type:"input",kind:"change"|"submit",value:string}` or `{type:"select",value:string}`. Input/select values are at most 16,384 characters. | `{ handled: boolean, reached?: plain object }` |
| `ui_client_fault` | `phase`: `load`, `render`, or `run`; `reason`: string up to 200 characters. | `{ handled: boolean }` |

The related remote parent-control routes are distinct from Client
`ui_client_press`. `ui_press` accepts `{ plugin, handle, key?, surface?, href?,
client_id? }`: `plugin` is any string, `handle` is an integer, `key` is an
unbounded string when present, `surface` defaults to `desktop`, `href` is at
most 2,048 string characters, and `client_id` is a nonempty safe 1–64 character
ID. It returns `{ handled, element? }`. `ui_input` accepts `{ plugin, handle,
kind, value, key?, component?, instance_id?, surface?, client_id? }`, where
`plugin` and `instance_id` are at most 256 characters, `kind` is `change` or
`submit`, `value` is at most 16,384 characters, `key` has no string bound,
`component` is a render-site name, and the remaining fields use the same
surface/client rules. `ui_select` has the same shape without `kind` and with a
required `value`. Both return `{ handled, element?, value? }`; the response
strings have no additional Zod maximum. Plain `plugin`, `key`, and `href`
strings have no nonempty, NUL, or safe-character restriction at this schema
layer. `handle` uses only Zod `int()` (no sign/range/safe-integer bound); a
value that passes the schema can still miss the held-handle lookup.

The SDK passes required `(plugin, handle)` to `E1e`. When present, `key`
becomes `element`, `component` remains `component`, and `instance_id` becomes
`requestId`; `client_id` is accepted but not forwarded. Missing `surface` is
parsed to `desktop` and then passed. `E1e` first looks up a live held closure
by plugin/handle/key. If that misses, it re-resolves only when key and
component are both present, using optional request ID to disambiguate multiple
drawings with the same key. Omitting key does not invent a key; it leaves the
direct held-handle lookup. The normalized hook event contains only
`{ plugin, element, component, requestId, surface }` plus `link: { href }` for
a link press, `kind` and `value` for input, or `value` for select. The handle
and `client_id` are not exposed to the hooks. Select rejects a value absent
from its currently held options before running hooks. An Input with no
optional `onInput` still has a wrapper which returns `undefined`; a matching
held Input reaches the bottom and is handled unless a hook withholds `next()`.
The wrapper invokes `onSubmit(value, event)`, `onInput(value, event)`, or
`onSelect(value, event)`, with the full normalized event as its second argument.
The hook argument checker requires a string `element`, string `component`,
the original `plugin` and `requestId`, and a recognized `surface`. It does not
cap `element` or require it to equal the drawn key; `component` only has to
remain a string. For press, a link must remain absent when the event had none,
or remain an object with a string `href` when it had one, but Native does not
pin that href's value. Input additionally pins `kind` and requires a string
`value`; select requires only a string `value`. These string fields have no
length limits in these hook argument checks.

`next(e)` applies those checks before passing a rewritten event beneath a hook.
The generic `next(e)` guard itself only requires a non-null object and returns
that object; the `ui.press`, `ui.input`, or `ui.select` site validator decides
the event fields. It does not impose a serialized-size or string-length cap.
The held callback remains bound to the handle captured from the original
drawing: the bottom receives the final rewritten event as data, while a changed
`element` does not retarget the callback to another drawing. Thus a hook may
rewrite `element` to an empty or different string and may replace an input or
select value with a string longer than the original control request limit; the
same original closure is called and the reached projection carries the final
element/value. `link.href` has the same unbounded-string rule when the original
press event already contains a link.

For `ui_press`, an absent `href` must resolve to a held Button; a present
`href` must resolve to a held Markdown action. Native checks the link against
that action's `pressableLinks` before hooks: an absent list permits any link,
while a present list requires a normalized URL match. The exact matcher first
prefixes `www.` with `http://` case-insensitively, then applies `Gpn` and
compares `new URL(value).href`; if URL parsing throws, it compares the raw
post-`Gpn` string. Thus parseable absolute URLs receive WHATWG canonicalization
(including scheme/host case, default ports, and dot segments); query and
fragment are not case-folded or reordered, though URL serialization can still
escape characters. Relative strings, empty strings, and
malformed URL strings take `Gpn`'s pass-through branch and remain exact,
case-sensitive strings. There is no scheme allowlist in this admission gate.
The exact prefix check in `sS` is the JavaScript regex `/^www\./i` (one
backslash in the source, case-insensitive). Because it prepends `http://`,
`www.Example.com:443/a/../b` normalizes to
`http://www.example.com:443/b`; port 443 is retained for HTTP. Both
`www.Example.com:80/a/../b` and `www.Example.com/a/../b` normalize to
`http://www.example.com/b` after default-port removal and dot-segment
normalization.
`Gpn` strips `file:` and then strips a `//` prefix. It removes a `localhost`
path prefix only with exact lowercase spelling; another host-like value becomes
a path segment and resolves under the contextual cwd. It splits the path from
the first query/fragment marker, attempts `decodeURIComponent` on the path,
normalizes a Windows drive prefix conditionally on the platform's
`path.isAbsolute`, resolves relative paths against `oe()` (async-local cwd,
then session/project cwd; on error, original cwd), and calls `pathToFileURL`.
Before returning that file URL, `RD` and `G6r` apply path guards against UNC or
device paths, network roots (`/net`, `/Network`), special volume paths, traversal
and control/format characters. The pinned Darwin-arm64 probe executes the exact
`Gpn`, `RD`, `kn`, and helper source with the platform's Node `path` and
`pathToFileURL`, while fixing only `oe()` to a test cwd; it performs no
filesystem read. It confirms `/tmp/a.md` and `file:///tmp/a.md` both normalize
to `file:///tmp/a.md`, and that `file:relative/a.md` resolves under the supplied
cwd. The Windows `kn` condition is source-pinned and directly checked with
`path.win32.isAbsolute`, but the Windows serializer itself was not run. The
same probe executes `sS`/`j9n` with Node's global `URL`, so URL serialization is
not a differential execution of Bun's URL implementation. A mismatch stops
before hooks and returns no handled result; Native does not recheck a link
after hooks rewrite the event.
A successful Markdown hook receives `link: { href }`; the response still
contains only `{ handled, element? }`.

The probe also starts the actual `crates/hooks/src/mods_worker.mjs` process and
uses its private `{ kind: "ui.press.preflight", href, pressableLinks?, cwd }`
RPC (with optional opaque `apiContextTicket` transport metadata) and same fixed
cwd. It compares Native `j9n` and worker admission for
all 1089 candidate/allowed-link pairs from a corpus covering absolute and
relative file inputs, `file:` hosts, `www.` and absolute web URLs, query and
fragment case, malformed percent escapes, unsafe paths, empty strings, and
raw/equivalent file URLs. It also checks absent versus empty allowlists. The
RPC returns only `admitted`, so this verifies the raw-normalization equivalence
relation rather than reading the worker's internal normalized strings. The
probe additionally asserts positive Native and actual-worker admission for
the `www.` HTTP pairs with explicit 443, default 80, case variation, and no
port; this guards against an all-false pairwise comparison. The expanded
corpus records the Native normalized-corpus digest and checks that the worker
source stays unchanged across execution; the tested worker SHA-256 was
`2c435f51526f2d35a35c3dafb2374e2a6f6e6943ffa01f298fd3e9cd0497410a`. One
additional positive RPC passes a non-empty opaque `apiContextTicket` alongside
the Native-positive 443 href pair. The ticket is current Host transport
metadata, not a Native `ui.press` field; this probe does not claim its value
matches a particular Host ticket format. Run it
with `node scripts/tests/mods_ui_client_control_289_probe.mjs
/tmp/harness-claude-code-2.1.289-darwin-arm64`.

At the held callback boundary, Native passes the final post-hook event to the
loaded module as `press(handle, event)`, and the module invokes its stored
callback with exactly that event as its sole argument. The Button helper stores
`onPress` directly. The Markdown helper stores a one-argument `onEvent` wrapper
that calls the user's `onLinkPress(event.link, event)`, so the user callback
receives the link object first and the full event second.
The Select options gate applies to the incoming value before the hook chain;
Native does not recheck a valid value after a hook rewrites it. A controlled
probe executes the pinned dispatcher and JSX Select wrapper with a stub chain:
an invalid incoming option skips hooks, while a valid option rewritten to a
value outside the list reaches `onSelect` and the result carries that rewritten
value. The hook chain itself is not executed by that probe.

`ui_client_press` is the Desktop control path for Client `Button`, `Input`, and
`Select` elements. The handler maps `client` to the parent Client key, keeps
`instance_id` as the parent render request ID, runs the appropriate
`ui.press`/`ui.input`/`ui.select` hook chain, and returns `reached` only when
that chain calls through to its native bottom callback. The response projection
is exact: press is `{ element }`; input and select are `{ element, value }`,
where `value` is the final string value at the hook-chain bottom. Input `kind`
is not returned in `reached`. A hook can withhold the bottom by answering
without `next()`; when the bottom is reached, the SDK has not invoked the
surface module's held callback. It only reports the element/value to the
correlated Desktop caller. Any subsequent local Client callback dispatch is a
separate external consumer responsibility; no Desktop consumer was executed in
this probe. The Native `zbr` drawing bridge passes an externally supplied
`runHeld` event through unchanged, and the surface VM passes `payload.event`
unchanged to the retained closure. No Native code maps the returned `reached`
object to a subsequent `runHeld` call, so Desktop Input/Select event
construction remains an external consumer contract; passing just
`reached.value` is not proven Native behavior. `ui_message` maps to the owning plugin's `ui.message` chain alone;
its Native message envelope is
`{ surface: "desktop", component, requestId, element: client, module, data }`,
and a response `props` can update the running Client instance. `ui_client_fault`
maps to the owning plugin's `ui.fault` chain alone and uses the same address
tuple; a stale address gets `{ handled: false }`. For all three, the SDK host
returns the result on the original correlated control request.

The SDK `ui_render` handler passes the wire viewport `{ columns, rows,
isFullscreen? }` through to the `ui.render` hook without renaming or deriving
its dimensions. There is no Desktop `ui_render` request builder in the captured
2.1.289 SDK source. The separate terminal renderer constructs its own viewport
from Ink dimensions (`conversationColumns` and `rows`); that is not evidence
of Desktop window geometry.

The invalidation system message is
`{ type: "system", subtype: "ui_invalidate", event: "ui.render", uuid,
session_id, instances? }`. When present, `instances` is an array of
`{ surface, component, instance_id }` and only those mounted render sites need
refreshing; when absent, any mounted render may be stale. It is pushed without
a control request ID.

The embedded surface runtime's `pointer`, `key`, and `runHeld` are host-side
callbacks, not SDK control request subtypes. The inspected SDK control handler
has `ui_client_press`, `ui_message`, and `ui_client_fault`, but no
`ui_client_pointer` or `ui_client_key` route. The concrete pointer/key producer
found in the pinned Native sources is the **terminal** Client renderer: Ink
pointer input becomes `{ type, x, y, fine?: { x, y }, button?, shift?, alt?,
ctrl? }`; enter/leave use the last local coordinates. Terminal keyboard input
becomes `{ key, ctrl?, shift?, meta? }` after its key mapping (Escape/Ctrl-C/
Ctrl-D are handled before the Client callback). In that terminal drawing path,
the renderer converts held tree closures to environment handles and calls
`runHeld(handle, event)` with the event unchanged. The SDK `ui_client_press`
handler does not call this terminal `runHeld` method. These terminal mappings
are not evidence for a Desktop Electron pointer/key payload; that external
Desktop producer remains unverified.

## Pinned source

| Native function/data | Byte range | SHA-256 |
| --- | ---: | --- |
| Client JSX constructor | `182690001..182691156` | `2186ea7d75b18bf2f3bee33cf85526c4488ddeb2c0017c2b9d7fef3fffa2fc75` |
| Plugin-root import guard (`f5o`) | `182564798..182565594` | `da25454fd89ccc833eecf1e8e59c48b5a18f71fff2afc355d4c32bff049ab59e` |
| Entry source loader (`Un`) | `182565720..182565800` | `c69ec13f6802f0d8efc56ee3e1e63d5dbad78cd925474f3f884e69218a9b0dbb` |
| Client surface module file-graph loader (`XEn`) | `182623124..182623361` | `c9e05cc981e04a01354ef3038537f53d6a67477b994059bb9f8ff5d09e3c7993` |
| Client props validator (`cBr`) | `186027762..186028588` | `5f36aab9810e3e5f33ad764891b2029541b3d99b10c8fda29ea6dacb26102d19` |
| `ui.resolve` Client ownership (`Te`/`Se`) | `182871426..182871916` | `7f7b2c725f18b0c6365e387a606b7b89de008fb1094f7332442c263e9db995e0` |
| `ui.resolve` returned element table conversion (`De`) | `182874857..182875380` | `f776f8d1b0e76bb0f72ad096f1dee68569dc23a78351bdf7652b28f7f2c98761` |
| JSX Client literal scanner (`bo`) | `182570297..182570467` | `2fb781e4db43dcb5d6ca88fdd0296e8f6dbc856008d62d4a06c7879a558d3402` |
| Client relative-path predicate (`_5r`) | `182563597..182563669` | `ffc36282ee4ef68f905efdbf702204fd5735f943e968624635ae62499ebb9b96` |
| Client source-relative to plugin-relative path (`So`) | `182571549..182571984` | `fe3b7949be319500ada3f0de551047f3d0080a87840286b803ec6ee0dfcd26d6` |
| Client source literal rewrite (`Uas`) | `182572082..182572323` | `6aeb259b0b326d4a400d0daa8666608d26cb814bbeb24b09341aa15be4bfe9f2` |
| Surface module export name selection (`$as`) | `182571149..182571461` | `dd7657efa37b1e3daee66d83e0a1c8224ecb295eef254f154e56136bb28fa81f` |
| normalized Client module list (`Or`) | `182572323..182572608` | `67d98b189290dbddbc2ebb67394e657378640025282349b0fef58d44d64c04f4` |
| Plugin Client path discovery/loader (`WEt`, `fHr`, `qEt`) | `188332817..188334058` | `b00b52f6aac7c2d493d1ef781287302cc7a593b22f7af307b158dcdfe1f7c8a0` |
| surface VM module resolver (`u5r`) | `182790916..182792417` | `9eb587278408ea1f2755207fb6bcf0f435516c9d3a7cc171c1a551fdc298c0bc` |
| surface VM source rewrite boundary (`u5r.H`) | `182791876..182792131` | `d2a67d21a470166ef79c9ea083b9c1e1bd01ef5993c7677e2805e58d86d5f444` |
| prepared surface-module record (`WEt`) | `188332817..188333254` | `84709940a4a88f09b18f50c81881ce869c42a2149308f27eaf300ffd36978eb8` |
| exact surface VM module lookup (`oe`) | `197825987..197826377` | `f879cb5af6c3b70eb95c2462147447bdcef3b0edb8680e922cee133521a24934` |
| Plain data validator (`Nt`) | `182719367..182720203` | `8bf245d1dce4a402642f612a7257fdd7c2c9ba85e8e5dcf709834bd12720192b` |
| Plain data budget (`wo`) | `182719015..182719210` | `f7ef610b75e1ef995ed00991d191d99982ca1af7b79766d3887c72bddd003576` |
| UI data/tree limits (`xZ`, `ZIe`, `e0e`) | `182644002..182644064` | `f12dc6399cfb627a8fdb110e656e7e5dfd7ba5665fd40ac5a080d21af2019fd8` |
| Surface limits projection (`h8n`) | `186093924..186093994` | `f07b896e59944311d5d7bfac5c4f788043594f5cbb97d9860a2c577406e5e534` |
| Embedded surface runtime source (`g8n`) | `186080973..186093793` | `fecdc1d1e8159a8552f15ef162b15a6c17722fb0a2c3f383efef47aef50f98d4` |
| Builtin classic JSX factory (`Ger`) | `182678123..182696349` | `5f9aca064bd0d9eec51d9f34f8e1be3399178d302144914bc1e19c282407e14c` |
| Builtin surface module template (`wO`) | `186093794..186093912` | `2eda803a1675d3d8e27f5b072fcbdce99ef7cc8dd6e7dace7851ea4e2db411df` |
| `claude:hooks-types` BunFS asset binding (`Etr`) | `182588479..182588546` | `9ee49de85c75b3f595ea6f89e8272e6791e1edc3e3c9d90d80699a9f24ab32d1` |
| Native border-style name set (`Was`) | `182629470..182631974` | `183bfbe7ae76be0fcf3a9beb170aa64b1b774e34c000cc55f49a3e80cf39451f` |
| Element tree validators (`lBr`..`xBr`) | `186025784..186051405` | `e8ab13236fb698a83caee1968d4f436ccf170f4e01f789e1f4c28d5b9b668e2c` |
| Surface module manifest/hash (`EO`) | `186093994..186094471` | `6d103388f4b0a54aceb896f749dd961dad9b7ee75579dd95bcae51c1c01c40ba` |
| Manifest stringifier import (`S`) | `185589940..185590050` | `6adda6c5e4b6908282d1751f362accb17eefaf87ca83c89bcb3e60dfa0369b62` |
| Manifest stringifier implementation (`S`) | `179403418..179403502` | `f7fd581fa06d8ed35ea81152ff3b735360b6195fddf61f5ae817dc689f942882` |
| Stringifier telemetry helper (`Re`) | `179404002..179404055` | `a8b72d4c36d4edc7eccdab218fa91a3ef4e972104fc632afefd2c13f2114d9c2` |
| Stringifier utility module source | `179356903..179425602` | `516a298c4608054e2528cc45014932c3dff3c62d796b37722cf279ba5b2fe2f8` |
| Client module registry (`RO`) | `186094471..186094823` | `3bff66fe97b5f011673cb3c81aceb6ec1bd10f156c13e6dca7a01bbeb6b543f4` |
| Render result `clientModules` hash projection (`d3`) | `186094847..186094985` | `1b12e003244ff81fe6fbc57516ea86fe30f14c49eae1a2079274de260f6de817` |
| `ui.message` envelope constructor (`tgn`) | `186094985..186095123` | `d2b82f3e56ff7008f3908d6d4367b199c7e29048f5dd51a7f1a34cb6b7b0ba4a` |
| `ui.message` Client post consumer (`O3`) | `186095545..186095785` | `721e7f6f9e54b0f85ae54d7a25498e4c1a251112b4f3d4d936d6caf5ab618cf2` |
| Native Client event constructors (`LP`/`BP`/`HP`) | `186019030..186021168` | `ec65c59273f5d262fa9764cfe083ea851b021ff3d025b11902f33f435b49c121` |
| Native Client `ui.message` envelope and dispatch (`tgn`/`N1t`/`O3`) | `186094985..186095785` | `d2a8de7676aab7a442912ac28233657f918eb165c0f1729036be5a0ab1d36ba0` |
| Native Client press/input/select hook projection (`I3`) | `186095785..186096798` | `1f52950cadf0e2be3155ab65c80dff5a70cdfa7460e626f91bd994da5e4ec412` |
| Native Client current-drawing identity gate (`isDrawn`) | `186075822..186077339` | `0c2387b6318b5e81fb4b9d7a16c768524c487074f955469d65d504869416f762` |
| Remote Link clickability gate (`OBr`) | `186053374..186053878` | `a89f41285ea1021e52049c0d157cdd9d962082cca36d5d867717d307114ca534` |
| Native hook-chain reducer (`qT`) | `182771204..182771783` | `3b44a868600fb9a171d87e2252ea0030d8031993c92e71571c390738cbb98f09` |
| Native per-hook next/call-through (`eg`) | `182774738..182777607` | `b4beb925b40fc51afa13d415d61892c2cf6ea8203154a2c4b2f9649beaa487d0` |
| Native `ui.resolve` hover-group restamping/capability (`Re`/`Ee`/`oe`) | `182871916..182874446` | `69db5968532d68a604a7536f2813d1192d16062c52d17f62650276c45a6a2201` |
| Native hover key allowlists (`X4r`) and value/relationship validator (`Y6t`) | `182640494..182642346`, `182642346..182644002` | `189e9b120f42998993db2274664433b3c59f4bbf56ada3e2596cd11152957952`, `1a54a31afaa03d776bf0cb0e090fba91942fcf6a625dbe8ed18617886d48490c` |
| Parent hover-scope ownership and context validator (`pBr`/`q9n`) | `186030557..186032757` | `f8bc9932d329ae4b5e759920bff183cae556a641d960a8a6cda9d551c29a8a9c` |
| Parent held callback event projection (`LP`/`HP`/`BP`/`qm`/`Vm`) | `186016324..186021168` | `3dc778e49d120ae893754570eafd3c7c7d0e2996bcf022bc96b09b2e95c7a85c` |
| Parent held callback storage and one-event invocation | `182876561..182882944` | `687ccdbbd650cff7fcf02d7a5ee88a017973ee5ff4086953f9b1fb4688868532` |
| Parent link URL path normalization (`Gpn`) | `185185005..185185446` | `7ecb73ed5a8b42081a9e2bf22bd0541774dacaa30bad766d9a8e15f71b252c4d` |
| Parent Gpn/RD producer module and path resolver imports | `185141352..185186008` | `5dff2c86ca15a3339ef1b49c35d5870b8e4285923f662e392769cb5eddd4ecf3` |
| Native path guard helpers (`Fn`, `Ol`, `PR`, `Of`, `KM`) | `179194648..179217677` | `5c40ce72bce681eebf3a96ef40fb8d8b5611db83b5d9ef3dd5dd3a2473bbf5a0` |
| Native file-URL path guards (`CXn`, `s7e`, `G6r`) | `185137529..185139203` | `9acf18b281f9841bc4d2e44daed753268b0d21a375e8b92e857a256feb2ab736` |
| Native path-control regex (`j4`) | `179170213..179177253` | `b275a7b1c9df4336655bcb2ea6b89656036f9dc76378245c8e94a7684f6886ec` |
| Native contextual cwd wrapper (`Sv`/`Rve`/`BKt`/`oe`) | `181139543..181140726` | `75c27cadae4b9280b7fbb579536af17ee824d50b87ed98f7e8ff11cac9331ae2` |
| Native current/original session cwd accessors (`As`/`_P`/`Ee`) | `179268614..179268680`, `179294415..179294474`, `179295134..179295167` | `7a68c31835916f8c66924b2adc340f2207ef6cb33f743bfa115fd18cf706053a`, `233960687c5e5d1973bfa0feef8c303435d9ed1805a64aa5777088debd4d7c57`, `683bc920c73a7905f3c8b95229f888060e15deecceeba078611802621c562a` |
| Parent link URL canonicalizer/matcher (`sS`/`j9n`, including `/^www\./i`) | `186019330..186019519` | `b89b60722610c6b6186053e70bdcadf17a11a1c369fa743b5ffb8f508e6e0a0f` |
| Parent Markdown/Button held-press identity and href gate (`Fmn`) | `186021168..186021896` | `59ec2f29a2fc34eb4d1bda5d44f52de7dd18e57de5cab9c9806d63332bed1714` |
| Parent `ui.press`/`ui.input`/`ui.select` hook argument validators (`Ge`/`Kr`/`mn`) | `182717731..182718287`, `182718287..182718475`, `182724883..182724999` | `07917ae03b9cd4192832b9bce73a9d027af3fdcceee32bf971eb40fd6781db31`, `aaefca5da1cf00d92d9e0474acc3a666ba861ce45353d259525c9de7f5321a21`, `5ab3fa1b41ecbe1c2ac907c34a0f044be62d5d474320c7f8328980107598e73a` |
| Generic `next(e)` object gate (`iGt`) | `180147250..180147395` | `06c05fd7a76fee84d7139947a944ab469112dd6d0ddcad4b6fa52a3b93e9f254` |
| Parent hook site validators and next-argument rewrite check (`ji`/`Hi`/`$i`/`lt`/`By`) | `182724999..182725631`, `182766211..182766661`, `182769578..182770499` | `137629b4b649e8206fb7b086a65be49b7bbb7609b8f359d9ff6a309aae479858`, `f008611818b5a06f09bce6fb421e937c46ad427f344a9a652fa616bb7c93de1b`, `1590dbaa7f986d05d3cb7337d32acbce847670f8a4b3d94c7a3773090c6d5008` |
| Generic `next(e)` object gate (`iGt`) | `180147250..180147395` | `06c05fd7a76fee84d7139947a944ab469112dd6d0ddcad4b6fa52a3b93e9f254` |
| `ui.render` tree and `clientModules` result | `186098500..186099268` | `bfe3b1722480f6203de10c29e765bf364284bbacb803d0439731d38c4f37ad8e` |
| Native render/client control schemas (`GAn` through `KW`) | `181089372..181102570` | `ee256e6e77748b2d75827de4228f70c55bd7d11c0a3102842c51da28d27123f8` |
| Native parent `ui_press` request/response schemas (`zAn`/`jW`) | `181093166..181094724`, `181094724..181095163` | `55d745248e067f85323bfd06f622554e9ef7d6f40b52590ae29a1dda06aa55e8`, `da9d11b05c398ac8642c4c4bb0facc9885a06915f35a63a9cfdccae90a8739df` |
| Native parent `ui_input` request/response schemas (`XAn`/`FW`) | `181102570..181104852`, `181104852..181105390` | `5832c72decd5420b82894ffe47bab65f138a5927ba84050eb9f471b1ff283d24`, `536d95c2ea5190bdfbbfe9123a1987b300f911f71a0d246a10271d50b8df9a07` |
| Native parent `ui_select` request/response schemas (`JAn`/`YW`) | `181105390..181107323`, `181107323..181107912` | `553d952e0ce2270c2fac969f50934ab7edf51f8713e0aeaecad294e06817d8f1`, `97be028937461d373e47b7410da7c4e06b9578dec3903bafb543cd05a509c8d4` |
| Native Zod unknown-key parser/default object factory | `181659962..181660990`, `181662427..181662752` | `62e4a9c6cb6a5408889593eed2edc7bf73484506a79e0ad805a55994a935530d`, `e3bc6f469229f61fa07c0d718083e3cf1e881420372d81faaf82089b069d072d` |
| Nested render objects (`Yw` bench, `Ww` viewport, `DW` on-screen, `Qw` keyed row) | `181085162..181085412`, `181085412..181086214`, `181086214..181086681`, `181108151..181108560` | `0969b917e76001dd59adb16afa7b9c9854cf3715e6e8bb225741302b917823a0`, `a077be97c98c92bf42677da89b74dc35abe602ef27ae03b17b9c116fe911d9f1`, `72555e350f6041534fd6d8d00d6b9dfb8f720674c659922a6415a1327b5a58b3`, `26e2f407f90c9979aef15200edbb51075cd22098b01ac3540d9a9457dda3b4a1` |
| SDK `ui_render` control handler | `206548795..206556461` | `6ebc028a516fa23815de8e7cd50f27a2a4726e8a46cfd62b3926cd8800ea4d2e` |
| SDK Native control-subtype allowlist | `205541917..205595868` | `14cd80dec1670d50d9ad51a6a3efab007e4944ec3bcf1549b3f951d9a6634c57` |
| SDK `ui_client_module` control handler | `206556461..206556839` | `f502c86eca39daf528578150aeadb2a5e83e12e0523b2cfaad7644ec892dacaf` |
| SDK `ui_client_press` control handler | `206556839..206557467` | `5c87b6306cc1bca41e39f3600fe8b98d112f4f8099b0b34ba10206fdf59d8299` |
| SDK `ui_message` control handler | `206557467..206557994` | `40474620d0196a0cf2528da405e729b56dae47fa6862c8787b6dc458bd25e57b` |
| SDK `ui_client_fault` control handler | `206557994..206558132` | `251e25e6115f445e9d3c9101815ddb15ab8ece4dfc97f56c7b89f095aa3605dc` |
| Parent `ui_press`/`ui_input`/`ui_select` SDK routes | `206550676..206551251`, `206551251..206552029`, `206552747..206553490` | `dd1eb4814d86bbc63137aa0cbe664ebfdcbda649d855707a69c4219fd318de9f`, `7bebcf4c22d021c1153b722a9ce60b7517c65f29c5d4f8a94b6d8b752f0502fe`, `5edfc8bac420d8d7faf4c1fc94c4ef3faaa75d26941bd1955beed970461dc300` |
| `ui_invalidate` system-event schema | `180875737..180877101` | `01a251bab24d3f9bee03259064fadf0abb3b2f7719e5edde016164a7b1d60751` |
| Native `ui_invalidate` SDK event projection | `206503857..206504073` | `0bc14b246d52f0a3c47b50f917363e1c5123b9492234a14219219ab791cef789` |
| Client manifest construction and hash preimage (`EO`) | `186093994..186094471` | `6d103388f4b0a54aceb896f749dd961dad9b7ee75579dd95bcae51c1c01c40ba` |
| Native parent-site key (`MD`) | `186063234..186063296` | `b1f076bb240612abc0f3e3a33865179a9f6af86958fe2e1022632aa3ef6ae7fc` |
| Native site-version eviction callback (`Q_`/`BC`) | `186063297..186063411` | `8a4760497ef9333f2d15123cc245bdb3c7c810d26005a0f1a29245df08caaefd` |
| Native site-version cache clear (`kS`) | `186062449..186062615` | `7898282dab4739fb842603776f482ac312f89215a715a234a86bbd37b2e4b696` |
| Native drawn registry, repeat-fault closure, and `QC = 1024` (`eO`/`WX`) | `186075934..186077175` | `7b9385ffb1f7026da547ae4ef1f10e863e486009390d0dc12a691e25c7f7bddf` |
| Native `faultRuns` key, `lO = 4096`, and eviction (`uO`/`cO`/`dO`) | `186077601..186078649` | `8ddc67706bf42056bb3c4d9915365e3185328e1983a69af629b2266b92cefa19` |
| JSX pragma prefix (`Xo`) | `182562071..182562147` | `4bc252b29c904a0f72caca03b45aa8a8cff10b2b4f75627f6bde64d5ae89f751` |
| Hook-source transpiler (`qEn`) | `182562693..182562890` | `832df48bf320790ff8b7dd6591c7fae4e2560989b942fd24621ee56d14cf4579` |
| Hook-source extension-to-loader map (`eir`, `itt`) | `179786018..179786212` | `40c1754c66b68f01a068205a89a15ae60e5a42eb633790c66b77d060cab6c73e` |
| Hook-source extension selector (`go`) | `182562147..182562203` | `eb80da9cf73d870e4cd26a268f587f012b476c2044ff32bf0ff1f4002db14c92` |
| Surface-module export selection (`$as`) | `182571149..182571461` | `dd7657efa37b1e3daee66d83e0a1c8224ecb295eef254f154e56136bb28fa81f` |
| Full Native Box/Text/Button/Input/Select/Link/Code/Markdown/Client tree schema (`Fw`) | `181078531..181085162` | `e519b2155e05d32163c9dc03dff0fe53ed2fad1e6a64b1b97bb88671345558b5` |
| Terminal pointer-event producer | `203330134..203331241` | `f15a05c3988c1d0df49d2bb0c9292a73229a69a9381101358732d09784042508` |
| Terminal key-event mapper | `203329524..203329721` | `088ac1efb801586aa1cc9de3a944d4802663f1f818ae3c4ff28e4030cfddf04a` |
| Terminal Client keyboard-focus/input producer | `203329721..203330134` | `18716f93d1c975d3bc6fa99583255823c4c64acab849b2ac71308149e4a3e535` |
| Client VM loader/mount bridge (`Go`) | `197823671..197827134` | `d861064afd254ecc64170b9e605048f9e746c515a78ccee9a2861bb91c2147da` |
| Plugin surface environment lifecycle (`qe`) | `197827134..197828466` | `9fca23a3c5f5780551309f2eb4eb43cfbe744bb1a0d9420f4dce437065f38100` |
| Desktop Client drawing/callback bridge (`zbr`) | `197828478..197835868` | `3f6f220aa7176322cf61eff70e8db40567f62ddec80d0e9abb07cbc8ea96e8ad` |
| Native client failure cleanup (`v`) | `197828673..197829074` | `c8f2ffcb5ae25621a4420856935e3716bad7b4cd15dcd5cb1febc26ee5c2b941` |
| Native client failure transition/snapshot/raised fault (`S`) | `197829242..197829538` | `55d8c5a1c3d6c4040c4fc4f91fc5ca5140055cf91dac93230437b1a15e0bf048` |
| Native runtime callback guard (`y`) | `197829587..197829670` | `fe358904dd23ed7048708c1e4fcc91da244d97a7117eba52a8ec224e77159a3b` |
| Native schedule guard (`me`) | `197831192..197831568` | `80daddb8c360fd3de4f869763fee83c80fd7689fa03ad8c61e583481e8cbe10a` |
| Native resize gate (`Z`) | `197833372..197833792` | `951156b87b9f2b83c57e09d415a5d8d0f1bd64dd553ae386e11d3a0f2b236c8b` |
| Surface VM instance unmount and timer cleanup (`Go.z`) | `197825881..197825987` | `7b2545389f1f9d12c39d928d2e424888e64bc2dc75b489e643a02c68d4baeb30` |
| Client manager terminal-frame fingerprint (`L`) | `197822808..197822896` | `cbe8ff162996b7b908496d0f2b7b6b773c4fdc691658769058d2a36f21444d7d` |
| Derived Client drawing request ID (`Gbr`/`Ee`) | `197820617..197820746` | `873e2b41e6e5242d62053feb46f17c316d7157d447b93072fde05df710c297ac` |
| Host Client identity/state (`re`/`ge`/`ve`) | `197821302..197822152` | `3e2773ea606c93e98db803862026629b3635417546bc0325c2e31b02405f7dac` |
| Native terminal Client subtree/layout/drawnAgain consumer (`no`) | `203330134..203331237` | `eddca432f4f56400c999fb198e7846bf880081701220a23c3ed0299fb3491b80` |
| Native terminal Client acquire wrapper (`ao`, `surface: terminal`) | `203331819..203332137` | `1ac9c55ef4fa67625339ac609df05783a6a000aab9b6f2c9c268598efb727b02` |
| Native terminal Client React boundary wrapper (`lo`) | `203332137..203332525` | `4150c0fadfbacefc374977ca6c38da022760f7617f01706b4604eae2704c0d27` |
| Native React error-boundary reset-key implementation (`BO`) | `203306028..203306890` | `2c771ae011478024c83d21d2694b6c692802edae6b602316588843b950b83711` |

The Node probe executes the exact Client JSX constructor, `_5r` and `So`
relative-path logic, and embedded `g8n` runtime with a controlled source
extension predicate, `h` constructor, and host callbacks. It covers path
normalization/rejection, parent Client payload, tree JSON shape, post/schedule/
timer/pointer/key calls, and nested-Client refusal. It does not execute Native
`Go`, the dynamic plugin filesystem/module graph, its real desktop `port`, or
an external Electron renderer. The module export selection, loader budgets,
environment reuse, frame IPC, and product callback consumer remain sourced
from code review of their pinned functions; the probe is not a Native
Desktop-app execution.

The control probe checks the exact SDK request/response schema and dispatcher
source ranges, checks the Native `ui_client_press`/`ui.message` event mapping,
executes the exact `S`, `qEn`, and `$as` functions with a controlled manifest,
transpiler stub, and export set, and exercises the exact extension loader map.
It confirms that the only concrete pointer/key producer found in the captured
sources is terminal Ink. It does not invoke Bun's actual transpiler, execute
the stateful plugin filesystem scanner or hook dispatch, or capture external
Desktop frame IPC/Product callback execution. The actual worker execution is
limited to the private `ui.press.preflight` RPC described above.

The full Native source chunks are also pinned in
`/tmp/claude-code-oracle-2.1.289/relevant-src/capture.json`, including
`src_182637043.js`, `src_182558590.js`, `src_182865546.js`,
`src_185588254.js`, `src_187181803.js`, and `src_197818196.js`.
