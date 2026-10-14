# Native `session.append` content projection comparison — 2.1.289

This bounded audit records the v218 Rust comparison. Its JSON
`rust_source_comparison` entries are historical observations, not a fresh scan
of the current implementation. The v240 migration now carries nullable Text
citations and optional ToolResult error flags through Core, history and SDK;
Root also corrected Agent duplicate-anchor replay. Current Rust compilation
and runtime verification are tracked in the integration validation ledger.

The original bounded audit compared the then-current Rust
`ConversationOrchestrator::rewrite_mod_append_blocks` implementation with the
Native 2.1.289 `q`/`D`/`te`/`oe` projection path. It does not edit production or
test sources and does not claim full hook-worker or filesystem execution.

Two concrete shape differences appear in the source and source-executed cases:

1. **Text citations:** Native `D` materializes every new or replaced text pick
   as `{type:"text", text, citations:null}`. Rust constructs
   `ContentBlock::Text { text }`, whose current type has no `citations` field;
   the persisted projection therefore omits that null field.
2. **Repeated ToolUse identity anchors:** Native `q` appends `a.get(key)` for
   each source block with that key. If a source row contains repeated
   `tool_use.id` values, a text overlay anchored after that key is replayed
   after every repeated source block. Rust stores overlays by key but removes
   the entry after the first source occurrence, so the same suffix appears
   only once.

The duplicate-ID case is structurally admitted by the Native append projection
contract: the block parser requires a string `type`, and the append response
guard checks an array of content blocks, immutable message fields, a matching
UUID, and equality with the last `next()` result. It does not enforce unique
ToolUse IDs. Whether an upstream provider ever emits duplicate IDs is outside
this probe; the Rust projection still differs for such an admitted row.

Other tested algorithmic cases matched the current Rust loop's source-level
behavior: image/document blocks are preserved when exactly echoed and placed
at the active anchor; the original order and values of keyed ToolUse blocks
win over reordered or modified incoming ToolUse blocks; changed unknown
`type#ordinal` blocks resolve to the original source block; omitted unkeyed
Image/Document blocks are dropped; and duplicate exact images consume each
source occurrence once. These are block-order/value observations, not a claim
of complete byte parity because every replacement Text has the citations gap
above.

The probe also checks a presence-sensitive ToolResult boundary: Native's
ToolResult schema makes `is_error` optional, and its projector preserves an
absent property when its logical value remains false. Rust's
`ContentBlock::ToolResult` stores a required `bool`, and the Mod input
projection emits `is_error` unconditionally. This is a gap if an absent field
reaches this Rust boundary; this audit does not establish whether the current
runtime producer always normalizes it to `false` first.

Native row anchors for the executed cases retained the synthetic outer UUID,
inner message ID, and timestamp. The functions execute the content projection
and `oe` row merge against a controlled source row; the shared assistant row
object is mutated in place, as documented in the earlier v1 audit.

The checked-in probe pins the Native darwin-arm64 binary at SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`. Key
windows include:

| Source | Byte range `[start, end)` | SHA-256 |
| --- | --- | --- |
| Source identity keys (`b`) | `[212340271, 212340573)` | `7fabf58dc79ef544ff29e40509423707055fd959ed6bea6a04aabf416fd0873b` |
| Anchor diff/reconstruction (`q`) | `[212341259, 212342141)` | `ce98e760d98fe115068e0954fc23886d6c05b9e25113555f3d8a8a263abe4b24` |
| ToolResult content and optional `is_error` projection | `[212342141, 212343137)` | `a23fc7ceb62860cbf7e32c2f9a610159f203c545eddb38909d782d4b8a595333` |
| New/replaced text pick (`D`) | `[212339434, 212339575)` | `782cacfa8155f91b44654c68ac529fa1ba267889c2e4c36a140ab422bf30ee87` |
| Assistant row projection/merge (`te`/`oe`) | `[212343598, 212344521)` | `5a719052e59ebddb4218a3608ef959fe36e2b049b47d3323431f35ed20b2cac3` |
| Native block schema: `type` string plus unknown-field catchall | `[179690862, 179690942)` | `baaa9d4a716af30b1a92ec1f3cfd6ddfdebd4c7e3f678c4ab7bd75cf58e05590` |
| Catchall schema is `ZodUnknown` | `[179689383, 179689411)` | `7721da81c579816a2c49b68106cafaa41ad0d914689f0fe50763dd5b3ca225ea` |

The comparison points in the current Rust source are
[`conversation/transcript.rs`](../../crates/orchestrator/src/conversation/transcript.rs):
`mod_append_content_block` and `rewrite_mod_append_blocks` (around lines
130–501), especially `ContentBlock::Text` construction around 429–431 and
single-consumption of an anchor overlay around 493–497. The text type itself is
in [`messages.rs`](../../crates/core/src/types/messages.rs) around lines 55–58.

Run the source-extracted Native vectors and compare them with the pinned
fixture:

```sh
node scripts/tests/mods_session_append_projection_289_v2_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
node scripts/tests/mods_session_append_projection_289_v2_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64 --check
```

The Node probe executes Native projector functions verbatim and supplies
controlled parser results for valid block shapes; the Native schema and
unknown-field catchall are separately pinned. The Rust comparison is static
source analysis, not compiled execution. No provider request, complete Native
hook worker, durable transcript writer, or Cargo test is run.
