// Replays only selected pure permission predicates from the official executable.
// No proprietary source is checked in, and the target executable is not run.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
assert.equal(sha256, "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d");
const ranges = [];
function source(start, end, lowerBound = 257000000) {
  const from = binary.indexOf(start, lowerBound);
  assert(from >= lowerBound, `Missing anchor ${start}`);
  const to = binary.indexOf(end, from + start.length);
  assert(to > from && to - from < 40000, `Invalid range ${start}`);
  ranges.push({ start: from, end: to, sha256: createHash("sha256").update(binary.subarray(from, to)).digest("hex") });
  return binary.subarray(from, to).toString("utf8");
}

const context = vm.createContext(Object.create(null));
vm.runInContext(`let provider, settings;
const Te=()=>provider, be=name=>settings[name];
const canonicalState={canonicalNameMemo:new Map};
const AL=()=>canonicalState,gt=()=>({}),DSe=()=>undefined,sS=x=>x,KW=()=>false,YNe=()=>undefined,Gn=x=>x;
`, context);
vm.runInContext(
  // Native catalogue and pure identity parser; service-owned overrides/model
  // knowledge/application-profile metadata are absent in this offline probe.
  "var " + source('Nwo={"//":', "function M(){return{bakedCatalog", 259000000) +
  source("function U(e){let t=new Map,a=new Map;for(let n of e.models)", "function ed(", 259000000) +
  "var " + source('P7=["us","eu","apac"', ',J=["us"', 258000000) + ";" +
  source("function Qd(e){let n=e.trim().toLowerCase()", "export{gAe", 258000000) +
  source("function Dy(e){e=KFt(e.toLowerCase())", "function KW(", 261000000) +
  source("function tl(e,n){if(e===void 0)", "function Xtn(", 261000000) +
  "var " + source('IA=["claude-opus-4-0"', "function iFn(", 261000000) +
  source("function UI(", "function bOr(", 259000000) +
  source("function cFn(", "function Ryo(", 261000000) +
  source("function Ugt(", "function Mwo(", 259000000) +
  source("var A_s=", "function sLn(", 262000000), context, { timeout: 1000 },
);
const native = vm.runInContext("({model:(m,p)=>{provider=p;return KP(m)},canonical:et,catalog:Nwo,consent:s=>{settings=s;return Ugt()},notice:EDt})", context);
vm.runInContext(
  source('var lU=["acceptEdits"', "var LAe=", 258000000) +
  source("function WI(", "function OB(", 258400000) +
  source("function yhn(", "function rBo(", 281000000), context,
);
vm.runInContext("const ghn=e=>e.isBypassPermissionsModeAvailable,n3=e=>e.isAutoModeAvailable;", context);
const parseMode = vm.runInContext("WI", context);
const nextMode = vm.runInContext("yhn", context);
const modeParsing = ["default", "manual", "acceptEdits", "plan", "bypassPermissions", "dontAsk", "auto", "bubble", "unknown", ""].map(input => ({ input, expected: parseMode(input) }));
const cycle = [];
for (const mode of ["default", "acceptEdits", "plan", "bypassPermissions", "dontAsk", "auto"]) {
  for (const bypass of [false, true]) for (const auto of [false, true]) {
    cycle.push({mode, bypass, auto, expected: nextMode({mode, isBypassPermissionsModeAvailable:bypass, isAutoModeAvailable:auto})});
  }
}
const models = [];
for (const model of [
  "claude-3-5-sonnet", "claude-opus-4-0", "claude-opus-4-1",
  "claude-opus-4-5", "claude-sonnet-4-0", "claude-sonnet-4-5",
  "claude-haiku-4-5", "claude-opus-4-6", "claude-sonnet-4-6",
  "claude-opus-4-7", "claude-opus-4-8", "claude-sonnet-5",
  "claude-haiku-4-6", "claude-haiku-5-5", "some-haiku-model", "gpt-6", "custom-model",
]) {
  for (const provider of ["firstParty", "anthropicAws", "anthropicGoogleCloud", "bedrock", "vertex", "foundry", "custom"]) {
    models.push({ model, provider, expected: native.model(model, provider) });
  }
}
const sources = ["userSettings", "projectSettings", "localSettings", "flagSettings", "policySettings"];
const consent = [];
for (let mask = 0; mask < 1 << sources.length; mask++) {
  const settings = Object.fromEntries(sources.map((source, i) => [source, { skipDangerousModePermissionPrompt: Boolean(mask & (1 << i)) }]));
  consent.push({ settings, expected: native.consent(settings) });
}
const rawModels = new Set([
  ...native.catalog.models.flatMap(row => [row.id, ...Object.values(row.provider_ids).filter(value => typeof value === "string")]),
  "opus", "sonnet", "haiku", "fable", "custom-model", "gpt-6", "openai/gpt-6", "provider/MY-CUSTOM-MODEL-20261104",
  "claude-opus-4", "claude-sonnet-4", "claude-opus-04-00", "claude-haiku-05-05", "claude-sonnet-4-99",
  "claude-opus-4-9-preview", "claude-opus-4-999-preview", "claude-sonnet-4-5-preview",
  "unknown/claude-sonnet-4-5", "prefix-claude-haiku-5-5-custom", "claude-haiku-5-50", "claude-haiku-5-5-custom",
  "claude-sonnet-4.5", "claude-3.5-sonnet", "claude-sonnet-4-5[2m]", " claude-sonnet-4-5 ",
]);
for (const row of native.catalog.models) {
  for (const suffix of ["[1m]", "[2m]", "-latest", "-fast", "-20261104", "@20261104", "-v2@20261104"]) rawModels.add(row.id + suffix);
  for (const region of ["us", "eu", "apac", "jp", "au", "us-gov", "global"]) rawModels.add(`${region}.anthropic.${row.id}-20261104-v1:0`);
  rawModels.add(`openrouter/anthropic/${row.id.toUpperCase()}[1m]`);
}
const normalization = [...rawModels].map(input => ({input, expected:native.canonical(input)}));
for (const model of rawModels) for (const provider of ["firstParty", "bedrock", "custom"]) models.push({model, provider, expected:native.model(model, provider)});

// Resolve the native built-in string constants from their exact byte locations.
// Plugin-only skill tools and browser/input predicates are deliberately outside
// this name-only built-in fixture. FWe is fixed to its public default false.
const constants = {
  ze: [260765007, "Read"], gr: [261731614, "Grep"], no: [261731600, "Glob"],
  XH: [263383103, "LSP"], Ps: [262567958, "ToolSearch"], sK: [263394944, "ListMcpResourcesTool"],
  "b$": [263384363, "RefreshMcpTools"], "r$": [266549398, "WaitForMcpServers"],
  FM: [266542685, "ReportFindings"], ov: [262564219, "TodoWrite"], HS: [265890243, "TaskCreate"],
  XQ: [266554706, "TaskGet"], tw: [265890263, "TaskUpdate"], vP: [266549082, "TaskList"],
  Ca: [265877861, "TaskStop"], _b: [265706836, "GetTask"], LYe: [266551370, "ConnectGitHub"],
  DE: [262568310, "EnterPlanMode"], n_: [262568537, "ExitPlanMode"],
  s0e: [266553712, "ShowOnboardingRolePicker"], NP: [263396469, "ReadMcpResourceTool"],
  tB: [263395640, "ReadMcpResourceDirTool"],
};
for (const [symbol, [offset, name]] of Object.entries(constants)) {
  const text = `${symbol}=${JSON.stringify(name)}`;
  assert.equal(binary.subarray(offset, offset + text.length).toString(), text);
  vm.runInContext(`const ${text};`, context);
}
vm.runInContext(`const C6t=null,R6t="SearchMcpRegistry",A6t="SuggestConnectors",x6t="ListConnectors",cUo=[];
const FWe=()=>false,mUo=new Set,L6t=new Map,zVe=new Set,qVe=new Set;`, context);
vm.runInContext("var " + source("fUo=new Set([ze", ",D6t=", 268000000) + ";" + source("function R3t(", "function F6t(", 268000000), context);
const safe = vm.runInContext("name=>R3t({name},{})", context);
const builtins = [...Object.values(constants).map(([, name]) => name), "SearchMcpRegistry", "SuggestConnectors", "ListConnectors", "TaskOutput", "SendMessage", "AskUserQuestion", "Agent", "Bash", "Edit", "Write", "WebFetch", "CronCreate"]
  .map(name => ({ name, expected: safe(name) }));

const fixture = {
  version: "2.1.318", executableSha256: sha256, sourceRanges: ranges,
  limitations: ["Native et/Dy with the baked catalogue: service-owned model overrides, served model knowledge and application-inference-profile metadata are absent.", "Built-in name-only allowlist only; plugin skill and browser predicates are excluded; FWe uses its public default false.", "Availability service state, fast-mode remote latches and live classifier execution are outside these pure-function probes."],
  models, normalization, consent, builtins, modeParsing, cycle, backgroundBypassNotice: native.notice,
};
const output = new URL("../../crates/permission/tests/fixtures/permission_modes_2_1_318.json", import.meta.url);
if (process.argv.includes("--write")) writeFileSync(output, JSON.stringify(fixture, null, 2) + "\n");
else assert.deepEqual(JSON.parse(readFileSync(output, "utf8")), fixture);
console.log(`Claude Code 2.1.318 permission mode oracle: ${models.length} model, ${normalization.length} normalization, ${consent.length} consent, ${builtins.length} built-in, ${modeParsing.length} parsing and ${cycle.length} cycle cases match`);
