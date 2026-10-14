# Server fallback QueryEvents and the public stream-json boundary

This source-pinned oracle compares the accepted server-fallback branch in the
official 2.1.288 and 2.1.289 macOS arm64 executables. It keeps Native's event
names and field casing intact; `assistant_continuation_begin` is not a Native
event name in either binary.

Run it with both verified reference binaries:

```sh
node scripts/tests/server_fallback_public_stream_289_oracle.mjs \
  /path/to/claude-code-2.1.288-darwin-arm64 \
  /path/to/claude-code-2.1.289-darwin-arm64 --check
```

The oracle fails closed on missing binaries, unexpected executable hashes,
changed source ranges, or fixture drift. The checked-in results are in
[`server_fallback_public_stream_2_1_289.json`](../../crates/permission/tests/fixtures/server_fallback_public_stream_2_1_289.json).
No Native executable or extracted source is checked in.

For an accepted fallback, both binaries have the same 379-byte producer branch
and source hash. It first appends discarded and compensation rows to the
internal queue, then yields `query_model_change` with `toModel`, yields one
`tombstone` for each discarded row and each compensation row with
`displayOnly: true`, and finally yields:

```js
{type:"refusal_continuation",phase:"begin",salvageText:J.text,join:"exact",replacesUuids:J.originals.map((ke)=>ke.uuid)}
```

That is an internal `QueryEvent` producer shape. Both releases' schema text
labels `tombstone` and `refusal_continuation` as `@internal`; the tombstone
description says its wire shape is pending a dedicated schema. The
`refusal_continuation` schema describes `salvage_text`, `uuid`, and
`session_id`, while the raw producer object uses `salvageText`, `join`, and
`replacesUuids` and has no top-level `uuid` or `session_id`. Neither release
contains a `type:R("query_model_change")` schema, and both have zero occurrences
of the literal `assistant_continuation_begin`.

The headless verbose stream-json loop takes each value as `Lt` (stamping a
`result` only) and calls `Q.write(Lt)` directly. The local output writer's
`write` method calls `trackWrite`, then hands the returned value to its line
writer. `trackWrite` stamps only `result` frames; for other types it performs
SDK-schema sampling and returns the same object. The source-driven probe passes
the three internal event types directly to this writer method and observes
those same types in its serialized output. It supplies `JSON.stringify` for the
imported serializer and a capturing line-writer callback. This proves that the
writer method itself does not filter them when handed such objects; it is not
an end-to-end CLI output capture.

The remaining path from the lower-level internal QueryEvent iterator through
the headless `EI`/`ID` producer to `Q.write` is not established by this oracle.
It therefore does not claim that the internal fallback events reach stdout or
that a complete CLI path drops them. The current evidence does not support
adding a new public `server_fallback_*` frame or renaming the internal begin
event. Keep that public-wire behavior open until the full path is source-pinned.

## Source ranges

All ranges are half-open byte offsets in the matching executable. The oracle
checks these hashes before running its small writer probe.

| Version | Source | Byte range | SHA-256 |
| --- | --- | ---: | --- |
| 2.1.288 | Accepted fallback event sequence | `195142622..195143001` | `4caa8b706acf3d45acf13588fd6337e076c84fcbb98078db54833c90b5748140` |
| 2.1.289 | Accepted fallback event sequence | `195457859..195458238` | `4caa8b706acf3d45acf13588fd6337e076c84fcbb98078db54833c90b5748140` |
| 2.1.288 | Internal `tombstone` schema | `180628383..180628846` | `8465c3e5ae1ad7311102699cd56fb0fc4608e63f35e795c996c7ec070e90050e` |
| 2.1.289 | Internal `tombstone` schema | `180891117..180891580` | `d906349e2b4fdf9b1f9c23b0511c800b1b5e8c320020ed01a90268e21b834e13` |
| 2.1.288 | Internal `refusal_continuation` schema | `180656306..180656711` | `13834ac216144f7adf654c2d23c895ea7c942741cee84e6a8957dfeafff01ab3` |
| 2.1.289 | Internal `refusal_continuation` schema | `180919040..180919445` | `32a9f7a9958bc247cda96200e0f72f3b36e2303462eee7306e983273edf60260` |
| 2.1.288 | Headless output frame before writer | `206106613..206106654` | `545f816299366fb00ca878c249249eb22629e65c2993b9dcdc4284f315ed6fdf` |
| 2.1.289 | Headless output frame before writer | `206442107..206442148` | `545f816299366fb00ca878c249249eb22629e65c2993b9dcdc4284f315ed6fdf` |
| 2.1.288 | `Q.write(Lt)` call | `206107200..206107228` | `e06379d7a063b2f79db0fa92aae653bbaa6aa2d75bd4c47a09fdb24e07ab1ffb` |
| 2.1.289 | `Q.write(Lt)` call | `206442694..206442722` | `bfee3435accd1e66eb223a616b4160d52fd7aec633d607af076ec5ba8dd3e339` |
| 2.1.288 | Local stream-json `write` method | `205247212..205247398` | `d1cec00c08b78b9613b64ad477769c717146c7807cad25d01aecca3281ad7272` |
| 2.1.289 | Local stream-json `write` method | `205581942..205582128` | `99da2da7ffa1f9da8c36797f396b7f9b95a383de655509b22c1264acb8770140` |
