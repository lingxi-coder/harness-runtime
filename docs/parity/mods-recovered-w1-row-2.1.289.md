# Claude Code 2.1.289 nonstream fallback rows and W1 sibling context

This note pins how a successful nonstream response after a stream failure is
represented before Agent tool execution. The source probe and captured evidence
are [the probe script](../../scripts/tests/mods_recovered_w1_row_289_probe.mjs)
and [its JSON fixture](mods-recovered-w1-row-2.1.289.json).

Run the check with the verified 2.1.289 binary:

    node scripts/tests/mods_recovered_w1_row_289_probe.mjs /tmp/harness-claude-code-2.1.289-darwin-arm64 --check

The binary is 229,616,464 bytes and has SHA-256
03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69.
The probe verifies this full hash and each byte-window hash. It runs no Cargo,
starts no CLI session, and makes no provider request.

## Nonstream response row

The actual nonstream adapter AGo sends the provider request with
stream:false and returns the whole response message object. On stream error,
the query fallback branch calls l8e; the 404 stream-creation fallback also
uses that same generator.

l8e passes the complete response through its refusal-aware content filter,
block normalizer, and batch-tool transform, then constructs one row gh from
the complete provider message: it spreads lr, replaces content with the
normalized full array, assigns apiBlockIndex:0, and creates one fresh outer
UUID. It pushes and yields gh once. The provider's inner message.id comes
from the spread lr; Native does not create one assistant outer row per block
on this nonstream path.

For ordinary surviving TextA/ToolA/TextB/ToolB content, with no special server
refusal-fallback filtering and no batch-tool decomposition, the transforms
retain the content-array order. Those scope conditions matter: dwn can
filter/replace server-refusal-fallback blocks, eFt normalizes or drops
malformed content, and HUt can expand batch-tool calls.

This differs from the streaming producer. A streaming
content_block_stop normalizes the single completed block [ha], wraps it in
one assistant row, stamps apiBlockIndex from the block index, and yields that
row. Thus the one-row finding applies to the actual l8e nonstream fallback;
it must not be generalized to streaming responses.

The Agent MC runQuery wrapper forwards query rows satisfying El with yield E;
El includes assistant and user rows (as well as progress and compact-boundary
system rows). It does not split a row's message.content into block rows.

## W1 current row and prior siblings

The query consumer chooses X = eo ?? M, then filters that row's content for
tool_use blocks and calls addTool(block, X) in content order. The executor
stores each entry as {block, assistantMessage: X}. The current block and
current row are then passed separately to W1.

W1 receives the current tool block and its assistant row as separate
arguments. For permission classification, it also receives
sameTurnToolUses. The executor builds that value by walking its tool-entry
array only up to, but not including, the current entry. It groups earlier
entries by assistantMessage object identity and returns shallow row clones
whose message.content contains only the earlier tool blocks for that row.
Completed entries are marked yielded but remain in the same executor tool
array; the builder therefore still sees an earlier sibling after its result
has been collected.

For one nonstream row containing TextA/ToolA/TextB/ToolB:

- W1 for ToolA receives an empty sameTurnToolUses.
- W1 for ToolB receives one partial clone of the current assistant row. It
  retains the row UUID and provider inner message ID, and its content contains
  ToolA only. Text blocks, ToolB itself, and later blocks are excluded.

The probe executes the exact extracted Native buildSameTurnToolUses method
in a Node VM against that ordered row and checks both results and that the
original row is unchanged. The provider adapter, l8e generator, query loop, and W1 are
source-pinned/static-inspected rather than executed. This is not an end-to-end
provider or Native CLI run.

## Pinned source windows

The JSON fixture contains every range and complete SHA-256. Key windows:

| Native code | Byte range | SHA-256 |
|---|---:|---|
| AGo nonstream provider adapter | [189272780,189275792) | 8c5767f055b2fe09e9da61a853edfb2d767cb845093b558c65da0de3a39da653 |
| l8e full-response row construction/yield | [189346377,189349484) | b130facacfb390b3d95f6981cb4692a01bc2a33d034a193cd80659e471f5b06d |
| dwn refusal-aware response content filter | [189341909,189343263) | 0a5e6c5014330afce1309b1d3c68e0aee3cec9f513faa18e37981c35e4aa18a3 |
| eFt content block normalization | [191356656,191359199) | e4c70457d8c391da8ceb14bee3706b23783b09f6d511a2cc0f2d93dc95ca81e4 |
| HUt batch-tool transform | [184985052,184985387) | 970be10d39bc63c462c134dbdd64437093d78544f248b0c37145aa7fe084f793 |
| stream-error fallback caller | [189390373,189391775) | 82a9d3ff4707c3767cd7dab5db759d9d012f5adab086019fbe85477b773d98bf |
| 404 fallback caller | [189391900,189393148) | adea093332e10571ea048149bde06ff63e0c479c760487e882f728b14b838494 |
| MC forwarded-row predicate | [194774097,194774244) | 90a7c47a62578e657003eed20fbaf183f32b1dfd8211bf2a8cbba89c9ebedd01 |
| streaming per-block row producer | [189368685,189370128) | aa8ce11178f40647830a76867eab1458645b378e24fbe5f6092a9f9042e4df50 |
| MC row pass-through | [194793235,194795722) | 9d13f060e3966a8a65a6d4a0d78a9db9004493e8ec48b3c1c2a69724d3b61f5a |
| query content-order enqueue | [195466181,195469377) | 380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c |
| executor addTool row association | [195350179,195351516) | 92b720b6a34784e97521f5d290052a628d34aa365dc8b674d7e6fde78f810b2a |
| completed entries remain in tool array | [195355929,195356509) | 39bd63336ce18174bef5b3867e0e6b721153fb64194a1ce144cd8418f2aa2bed |
| buildSameTurnToolUses | [195353804,195354117) | 813ea3c0b71b8e92bb9097ba2d7d23cc6a3de6f13c4a221af698db96f8bfd835 |
| executor W1 call | [195354700,195355020) | 49f7d225d78efab574e0afb853cfadacad167ff88b4777d39f8611d5c3df15b9 |
| W1 passes sibling context to permission helper | [188831280,188831680) | 21cad4d61d3ecdc8f48224da1577da2f005c9eb0ae48f123510c21fd81c1c792 |
