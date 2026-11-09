// Execute hash-pinned 2.1.319 threshold functions against synthetic model limits.
// KS only supplies the caller-selected window; no provider catalog is simulated.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd")
  throw Error("Unexpected Claude Code baseline");
const ranges = [
  { name: "L4", offset: 271032041, bytes: 139 },
  { name: "IQe", offset: 271032180, bytes: 86 },
  { name: "DQe", offset: 271032266, bytes: 323 },
  { name: "AZr", offset: 271034333, bytes: 14 },
  { name: "Ure", offset: 271038297, bytes: 102 },
  { name: "jdt", offset: 271038399, bytes: 79 },
];
const source = ranges.map(({ offset, bytes }) => binary.subarray(offset, offset + bytes).toString("utf8")).join("\n");
const native = vm.runInNewContext(source + String.raw`
let enabled=true;
const qu=()=>enabled;
const KS=(model,window)=>({window:Math.min(model.contextWindow,window??model.contextWindow)});
function run(input){
 enabled=input.enabled??true;
 const model={contextWindow:input.contextWindow,maxOutputTokens:input.maxOutputTokens};
 const config={enabled,precomputeBufferFraction:0.2,testPctOverride:input.pctOverride,testBlockingOverride:input.blockingOverride};
 const effectiveWindow=Ure(model,input.autoCompactWindow);
 const blockingWindow=jdt(model);
 const threshold=L4(effectiveWindow,config);
 return {effectiveWindow,threshold,precomputeThreshold:IQe(effectiveWindow,config),blockingLimit:input.blockingOverride??blockingWindow-3000,
  levels:input.tokens.map(tokens=>({tokens,...DQe(tokens,effectiveWindow,config,blockingWindow)}))};
}
({run});`, Object.create(null), { timeout: 1000 });
const cases = [
  {name:"default-200k",contextWindow:200000,maxOutputTokens:32000,tokens:[146999,147000,166999,167000,176999,177000]},
  {name:"native-1m",contextWindow:1000000,maxOutputTokens:128000,tokens:[946999,947000,966999,967000,976999,977000]},
  {name:"small-output-limit",contextWindow:200000,maxOutputTokens:8000,tokens:[158999,159000,178999,179000,188999,189000]},
  {name:"configured-window",contextWindow:1000000,maxOutputTokens:128000,autoCompactWindow:200000,tokens:[146999,147000,167000,977000]},
  {name:"configured-window-capped",contextWindow:200000,maxOutputTokens:32000,autoCompactWindow:500000,tokens:[166999,167000]},
  {name:"percentage-50",contextWindow:200000,maxOutputTokens:32000,pctOverride:50,tokens:[69999,70000,89999,90000,177000]},
  {name:"percentage-capped-99",contextWindow:200000,maxOutputTokens:32000,pctOverride:99,tokens:[166999,167000]},
  {name:"percentage-invalid-zero",contextWindow:200000,maxOutputTokens:32000,pctOverride:0,tokens:[166999,167000]},
  {name:"percentage-invalid-over-100",contextWindow:200000,maxOutputTokens:32000,pctOverride:101,tokens:[166999,167000]},
  {name:"disabled-ignores-window",contextWindow:200000,maxOutputTokens:32000,autoCompactWindow:100000,enabled:false,tokens:[146999,147000,159999,160000,167000,177000]},
  {name:"blocking-override",contextWindow:200000,maxOutputTokens:32000,blockingOverride:100000,tokens:[99999,100000,167000]},
].map(input=>({...input,expected:native.run(input)}));
const fixture={version:"2.1.319",sha256,ranges,scope:"Pure threshold formulas with artifact-default 20% precompute buffer; synthetic model limits and selected windows. Provider-specific window selection is outside this fixture.",cases};
const target=fileURLToPath(new URL("../../crates/compaction/tests/fixtures/claude_2_1_319_thresholds.json",import.meta.url));
const output=JSON.stringify(fixture,null,2)+"\n";
if(process.argv.includes("--check")){
 if(readFileSync(target,"utf8")!==output)throw Error("Threshold oracle fixture differs");
 console.log("Claude Code 2.1.319 threshold oracle matches "+sha256);
}else{
 writeFileSync(target,output);
 console.log(target);
}
