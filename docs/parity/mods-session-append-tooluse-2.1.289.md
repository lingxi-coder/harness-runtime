# Native `session.append` ToolUse projection — Claude Code 2.1.289

The pinned Native contract accepts an assistant `session.append` answer whose
`message.content` contains only replacement Text, provided the answer is the
row returned by the last `next()` call, keeps that row's UUID, and preserves
the immutable `message.type`, `name`, `role`, and `isMeta` fields. The response
guard does not require a plugin to echo existing `tool_use` blocks.

Acceptance is separate from storage projection. Native's `q`/`D`/`te` helpers
map accepted blocks back onto the source content. A newly supplied Text block
replaces the source text, while an omitted source `tool_use` is represented as
a raised source-block pick and reconstructed from the original row. The
`oe` helper merges the projected content into the same assistant row object in
place. For the controlled source-executed case, the stored content is:

```json
[
  {"type":"text","text":"Mod accepted text only","citations":null},
  {"type":"tool_use","id":"toolu_original","name":"Bash","input":{"command":"echo original"}}
]
```

So a Text-only hook answer cannot erase the original ToolUse from the Native
stored row. The in-place merge is a separate fidelity detail: the through
wrapper awaits the append side effect and then yields the same event object.
Its text content can therefore reflect the accepted projection before the
query consumer receives it. The ToolUse block itself is reconstructed from the
source and remains unchanged; the query's tool enqueue filters that block from
the row it receives. Describing the entire raw query row as byte-for-byte
unchanged would be inaccurate.

The source pins are tied to the local 2.1.289 darwin-arm64 binary with SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`:

| Contract | Byte range `[start, end)` | SHA-256 |
| --- | --- | --- |
| Response shape, immutable fields, UUID/`next()` guard | `[182727018, 182728316)` | `481f4920b5e69666b7f31c884d9869b4b8bf0cc4f219a9c82dd3a6bdff4d13de` |
| Content-diff, source-block picks, projection and assistant merge | `[212339434, 212344890)` | `44df3543c990b61bc0f06952210285cc98ff497b908235c94d2152888b7b91dc` |
| Source block clone used by row projection | `[188334648, 188334735)` | `f55199628115b3aa3444ebef9eed0d76f5509c599107172e4a0eb3f1069f3e08` |
| Eligible append rows and same-object through wrapper | `[195330200, 195331847)` | `90b6d38d60f49c1688b8bcbbe510db76bc3e5f155f33f1700a88f4d8537a65ed` |
| Query row selection and ToolUse enqueue | `[195466181, 195469377)` | `380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c` |

The executable probe extracts and SHA-checks those source windows, then runs
the exact guard and row projection helpers in isolated Node VM contexts:

```sh
node scripts/tests/mods_session_append_tooluse_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
node scripts/tests/mods_session_append_tooluse_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64 --check
```

The probe supplies controlled valid-block parser functions at the imported
schema boundary; it does not launch the complete Native hook worker, commit a
filesystem transcript, or test write scheduling. The query enqueue conclusion
is source-pinned, not executed through a full Native request.

Machine-readable window hashes and observed VM results are in
[`mods-session-append-tooluse-2.1.289.json`](mods-session-append-tooluse-2.1.289.json).
