// Execute only hash-pinned pure parser and fast-allow ranges from 2.1.318.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import vm from "node:vm";
import * as zlib from "node:zlib";
import { execFileSync } from "node:child_process";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d")
  throw Error("Unexpected Claude Code 2.1.318 artifact");
const ranges = [];
function range(startAnchor, endAnchor, from = 270519000, maxBytes = 4000) {
  const start = binary.indexOf(startAnchor, from);
  const end = binary.indexOf(endAnchor, start + startAnchor.length);
  if (start < 0 || end <= start || end - start > maxBytes) throw Error(`Missing ${startAnchor}`);
  ranges.push([start, end]);
  return binary.subarray(start, end).toString("utf8");
}
const parsers = range("function vot(", "function uot(");
const reason = range("function XSn(", "function QSn(");
const fast = range("let Us=_s(Qt.content)", "if(rn=Pa===!0", 270521700);
const context = vm.createContext({ _s: (content) => content[0].text });
const native = vm.runInContext(
  parsers + reason + `;({parse:mot,reason:XSn,fast:(Qt)=>{let Ee,Nt,wn;${fast}return Pa===false;},stageOne:(Qt)=>{let Ee,Nt,wn;${fast}return {block:Pa,refused:Pa===null&&Qt.stop_reason==="refusal"};}})`,
  context, { timeout: 1000 },
);
const texts = [
  "<block>yes", "<block>no</block>", "<BLOCK>YES", "<block>yeſ",
  "<block>no中文", "<block>yes中文", "<block>yes_", "<block>yes0",
  "<thinking><block>yes</thinking><block>no", "<thinking>pending <block>no",
  "<block>no<block>no", "<block>yes<block>no", "<block>yes<reason>\ufeffreason\ufeff</reason>",
  "<block>yes<reason>\u0085reason\u0085</reason>",
];
const parseCases = texts.map((text) => ({ text, block: native.parse(text), reason: native.reason(text) }));
const fastTexts = ["<block>no", "<block>no</block>", " \ufeff<block>no\ufeff ", "<BLOCK>NO", "<thinking>safe</thinking><block>no", "Looks safe <block>no", "<block>no<reason>safe</reason>", "<block>no中文"];
const fastCases = fastTexts.flatMap((text) => ["end_turn", "stop_sequence", "max_tokens", "refusal"].map((stopReason) => ({ text, stopReason, allow: native.fast({ content: [{ text }], stop_reason: stopReason }) })));
const stageOneRefusals = ["<block>yes", "<block>no", "unparseable"].map((text) => ({text,
  ...native.stageOne({content: [{text}], stop_reason: "refusal"})}));
const timeoutSource = range("var jqe=", "var Wqe=", 268463867);
const nativeTimeout = vm.runInContext(timeoutSource + ";nzt", context, { timeout: 1000 });
const timeoutCases = [0, 49999, 50000, 50001, 99999, 100000, 100001, 250000, 300000, 350000, 350001, 1000000].map((tokens) => ({ tokens, timeoutMs: nativeTimeout(tokens) }));
// Native chunk-6t34pqw1.js loads this zstd asset. These exact bytes come from
// the __BUN file map, not a user-supplied or guessed policy body.
const framing = range("function GYr(", "function ", 270433512, 60000);
const hostStart = binary.indexOf("var rzt=", 268465000);
let hostEnd = hostStart + "var rzt=".length + 1;
for (; hostEnd < binary.length; hostEnd++) {
  if (binary[hostEnd] === 92) { hostEnd++; continue; }
  if (binary[hostEnd] === 39) break;
}
if (hostStart < 0 || hostEnd - hostStart > 4000) throw Error("Missing host-context framing");
ranges.push([hostStart, hostEnd + 1]);
const promptContext = vm.createContext({ cP: () => false });
const base = vm.runInContext(binary.subarray(hostStart, hostEnd + 1).toString("utf8") + ";" + framing + ";GYr()", promptContext, { timeout: 1000 });
const assetRange = [315029580, 315061280];
const compressedPolicy = binary.subarray(...assetRange);
if (createHash("sha256").update(compressedPolicy).digest("hex") !== "f21d56434dd4b7beab7fc4a281aaff03289b2d5d4e6edc20086be3437e246ab9") throw Error("Unexpected compressed native policy");
const policyBytes = zlib.zstdDecompressSync ? zlib.zstdDecompressSync(compressedPolicy) : execFileSync("zstd", ["-d", "-c"], { input: compressedPolicy, maxBuffer: 1000000 });
if (createHash("sha256").update(policyBytes).digest("hex") !== "bfb494467ebd7003029b1d5da21f99a598a6238162d5b7933329abdf45625088") throw Error("Unexpected decompressed native policy");
const policy = policyBytes.toString("utf8");
const wrapPolicySource = range("function zYr(", "class KSn", 270478194);
const wrappedPolicy = vm.runInContext(wrapPolicySource + ";zYr", promptContext, { timeout: 1000 })(policy);
const templatePath = new URL("../../crates/permission/src/bundled/auto_mode_318_template.txt", import.meta.url);
const systemPath = new URL("../../crates/permission/src/bundled/auto_mode_318_system.txt", import.meta.url);
const template = base.replace("<permissions_template>", () => wrappedPolicy)
  .replace("<cross_session_messages_rule>", "")
  .replaceAll("Claude Code", "LingXi").replaceAll(".claude", ".lingxi") + "\n";
const slotRenderer = range("function UBn(", "function WIr(", 258073700);
const system = vm.runInContext(slotRenderer + ";jIr", promptContext, { timeout: 1000 })(template, { settingsDenyBlock: "" }, { collapseEmptySlotLines: false });
if (process.argv.includes("--write-prompts")) {
  writeFileSync(templatePath, template);
  writeFileSync(systemPath, system);
} else if (readFileSync(templatePath, "utf8") !== template || readFileSync(systemPath, "utf8") !== system) throw Error("Native prompt composition changed");
const fixture = { version: "2.1.318", sha256, ranges, parseCases, fastCases, stageOneRefusals, timeoutCases,
  framing: { nativeBytes: Buffer.byteLength(base), assetRange, policyBytes: policyBytes.length, policySha256: createHash("sha256").update(policyBytes).digest("hex"), templateSha256: createHash("sha256").update(template).digest("hex"), systemSha256: createHash("sha256").update(system).digest("hex") },
  scope: "Pure XML parsers, non-severity fast-stage allow, and native public/default prompt including its decompressed shipped policy. Brand names are replaced and cross-session host context is absent for the local default host; no provider execution" };
const path = new URL("../../crates/permission/tests/fixtures/permission_classifier_2_1_318.json", import.meta.url);
const rendered = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--write")) writeFileSync(path, rendered);
else if (readFileSync(path, "utf8") !== rendered) throw Error("Native classifier fixture changed");
console.log(`Claude Code 2.1.318 classifier: ${parseCases.length} parser, ${fastCases.length} fast-allow, ${timeoutCases.length} timeout cases and shipped prompt match`);
