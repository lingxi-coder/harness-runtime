// Replay the native usage anchor and server-cleared-token accounting.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd")
  throw Error("Unexpected Claude Code baseline");
const ranges = [
  { name: "Tde", offset: 270960333, bytes: 490 },
  { name: "SI", offset: 270960823, bytes: 57 },
  { name: "n$", offset: 270960880, bytes: 93 },
  { name: "Jsn", offset: 270960973, bytes: 119 },
  { name: "Zsn", offset: 270961092, bytes: 303 },
  { name: "mne", offset: 270961748, bytes: 526 },
  { name: "Qve", offset: 270962274, bytes: 344 },
  { name: "E7e", offset: 270962618, bytes: 72 },
  { name: "Zv", offset: 270962690, bytes: 68 },
  { name: "yN", offset: 270962758, bytes: 209 },
  { name: "oW", offset: 270962967, bytes: 107 },
  { name: "Ff", offset: 270964792, bytes: 98 },
  { name: "v7e", offset: 270964890, bytes: 38 },
  { name: "Nre", offset: 270964928, bytes: 131 },
  { name: "nin", offset: 270965059, bytes: 275 },
  { name: "C7e", offset: 270966116, bytes: 300 },
];
const source = ranges.map(({ offset, bytes }) => binary.subarray(offset, offset + bytes).toString("utf8")).join("\n");
const native = vm.runInNewContext(source + String.raw`
const Mi="<synthetic>",vYe=new Set;
const qr=row=>row.type==='system'&&row.subtype==='compact_boundary';
const kJi=()=>false;
const Bh=rows=>rows.reduce((sum,row)=>sum+Math.floor((textOf(row).length+2)/4),0);
function textOf(row){const content=row.message?.content;return typeof content==='string'?content:(content??[]).map(block=>block.text??'').join('')}
function run(rows){
 const liveTokens=Ff(rows,4),serverClearedTokens=Nre(rows);
 return {anchor:C7e(rows),liveTokens,serverClearedTokens,triggerTokens:liveTokens+serverClearedTokens};
}
({run});`, Object.create(null), { timeout: 1000 });
const user = (text) => ({ type: "user", message: { content: text } });
const assistant = (usage, text = "aaaa", id = "message-1", extra = {}) => ({
  type: "assistant", message: { id, model: "mock-model", content: [{ type: "text", text }], usage, stop_reason: "end_turn", ...extra },
});
const usage = { input_tokens: 120000, cache_read_input_tokens: 20000, cache_creation_input_tokens: 10000, output_tokens: 2000 };
const boundary = { type: "system", subtype: "compact_boundary" };
const edits = { applied_edits: [{ type: "clear_tool_uses_20250919", cleared_input_tokens: 25000, cleared_tool_uses: 5 }] };
const cases = [
  { name: "no-usage-estimate", rows: [user("aaaaaaaa"), { type: "assistant", message: { content: "bbbbbbbb" } }, user("cccc")] },
  { name: "usage-includes-input-cache-and-output", rows: [user("aaaaaaaa"), assistant(usage)] },
  { name: "usage-plus-tail-estimate", rows: [user("aaaaaaaa"), assistant(usage), user("bbbbbbbb")] },
  { name: "zero-usage-skipped", rows: [assistant(usage), assistant({ input_tokens: 0, output_tokens: 999 }, "bbbb", "message-2"), user("cccc")] },
  { name: "zero-input-only-estimate", rows: [assistant({ input_tokens: 0, output_tokens: 999 }), user("bbbb")] },
  { name: "latest-valid-usage-wins", rows: [assistant(usage), user("bbbb"), assistant({ input_tokens: 20000, output_tokens: 200 }, "cccc", "message-2"), user("dddddddd")] },
  { name: "same-api-message-anchor-rewinds", rows: [assistant({ input_tokens: 10, output_tokens: 1 }, "aaaa", "shared"), assistant(usage, "bbbbbbbb", "shared"), user("cccc")] },
  { name: "boundary-clears-usage-anchor", rows: [assistant(usage), boundary, user("aaaaaaaa"), user("bbbb")] },
  { name: "post-boundary-usage-anchor", rows: [assistant(usage), boundary, user("aaaa"), assistant({ input_tokens: 10000, output_tokens: 100 }, "bbbb", "new"), user("cccc")] },
  { name: "unmetered-usage-skipped", rows: [assistant(usage), { ...assistant({ input_tokens: 999999, output_tokens: 100 }, "bbbb", "unmetered"), isUnmetered: true }, user("cccc")] },
  { name: "fallback-iteration-normalized", rows: [assistant({ ...usage, iterations: [{ type: "message", input_tokens: 80000, output_tokens: 1000, cache_creation_input_tokens: 0, cache_read_input_tokens: 4000 }, { type: "fallback_message", input_tokens: 50000, output_tokens: 300, cache_creation_input_tokens: 2000, cache_read_input_tokens: 1000 }, { type: "compaction", input_tokens: 900000, output_tokens: 8000, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 }] })] },
  { name: "server-cleared-tokens-added", rows: [assistant(usage, "aaaa", "message-1", { context_management: edits }), user("bbbb")] },
  { name: "server-cleared-client-estimate-clamped", rows: [assistant(usage, "aaaa", "message-1", { context_management: { ...edits, client_estimated_cleared_input_tokens: 15000 } })] },
  { name: "server-cleared-measured-client-estimate", rows: [assistant(usage, "aaaa", "message-1", { context_management: { ...edits, client_estimated_cleared_input_tokens: 30000, client_estimate_measured_from_prompts: true } })] },
  { name: "server-cleared-stops-at-boundary", rows: [assistant(usage, "aaaa", "message-1", { context_management: edits }), boundary, user("bbbb")] },
].map((input) => ({ ...input, expected: native.run(input.rows) }));
const fixture = {
  version: "2.1.319", sha256, ranges,
  scope: "Native usage anchor, scalar/iteration usage normalization, compact boundary reset, and server-cleared metadata. Synthetic UTF-16/4 tail estimator; no attachment/tokenizer/synthetic-error-message behavior or provider request execution.",
  cases,
};
const target = fileURLToPath(new URL("../../crates/compaction/tests/fixtures/claude_2_1_319_usage.json", import.meta.url));
const output = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output) throw Error("Usage oracle fixture differs");
  console.log("Claude Code 2.1.319 usage oracle matches " + sha256);
} else {
  writeFileSync(target, output);
  console.log(target);
}
