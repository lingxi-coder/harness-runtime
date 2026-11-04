// Replay hash-pinned 2.1.318 compact split/retry functions with a synthetic model.
// Runtime flags use artifact defaults; the experimental tool-result omission flag is off.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (
  sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d"
)
  throw Error("Unexpected baseline");
const ranges = [
  { name: "zRt", offset: 269668038, bytes: 268 },
  { name: "VRt", offset: 269668306, bytes: 75 },
  { name: "_cn", offset: 269686567, bytes: 567 },
  { name: "lRe", offset: 269690290, bytes: 236 },
  { name: "uRe", offset: 269690526, bytes: 93 },
  { name: "vcn", offset: 269691649, bytes: 3449 },
  { name: "gZe", offset: 269695098, bytes: 135 },
  { name: "Zur", offset: 269695233, bytes: 77 },
  { name: "gcr", offset: 269695310, bytes: 114 },
  { name: "Ccn", offset: 269695424, bytes: 38 },
  { name: "bZe", offset: 269695475, bytes: 144 },
  { name: "hZe", offset: 269695619, bytes: 59 },
  { name: "wcn", offset: 269695678, bytes: 144 },
  { name: "_cr", offset: 269695822, bytes: 281 },
  { name: "bcr", offset: 269696103, bytes: 153 },
  { name: "pZe", offset: 269696256, bytes: 651 },
  { name: "Tcn", offset: 269696921, bytes: 51 },
  { name: "Ecn", offset: 269696972, bytes: 56 },
  { name: "mRe", offset: 269697028, bytes: 96 },
  { name: "yRe", offset: 269697124, bytes: 7184 },
  { name: "mZe", offset: 269704308, bytes: 115 },
  { name: "hRe", offset: 269992362, bytes: 408 },
  { name: "PZe", offset: 269995369, bytes: 84 },
  { name: "Jte", offset: 269995453, bytes: 119 },
  { name: "MZe", offset: 269995572, bytes: 84 },
  { name: "EBe", offset: 272553978, bytes: 184 },
];
const source = ranges
  .map(({ offset, bytes }) =>
    binary.subarray(offset, offset + bytes).toString("utf8"),
  )
  .join("\n");
const promptStart = binary.lastIndexOf('var rZe="an <analysis>');
const promptEnd = binary.indexOf("function Cmi(", promptStart);
const promptSource = binary.subarray(promptStart, promptEnd).toString("utf8");
const harness = String.raw`
const IAt="test-timeout",Vm="artifact",uv="remote-control",Qte=3,Tcr=10000,TZe=5,vZe=5000,CZe=25000,ycr=1000,D4=[],NTt=new Set;
const aRe={ids:new Set,madeByToolName:new Map,refused:0,withMedia:0,textLength:0};
const t=()=>{},a=()=>{},l=()=>{},m=()=>{},ce=x=>x,p=x=>x,S=x=>x;
const pmr=()=>true,vis=()=>false,Xvi=()=>true,Jvi=()=>false,ae=()=>false;
const Zte=()=>undefined,lj=()=>undefined,QX=()=>[],kcn=()=>undefined,Xte=()=>undefined,wRe=()=>undefined;
const $s=x=>x,h_=()=>false,DH=row=>['[Request interrupted by user]','[Request interrupted by user for tool use]','No response requested.'].includes(textOf(row));
const eg=row=>Array.isArray(row.message.content)&&row.message.content.some(block=>block.type==='tool_result');
const xc=text=>Math.floor((text.length+2)/4);
const Mh=rows=>rows.reduce((total,row)=>total+xc(textOf(row)),0);
const kY=x=>x,rde=x=>x,IZe=x=>x;
const imn='[earlier conversation truncated for compaction retry]';
const Ie=({content,isMeta})=>({type:'user',isMeta,message:{content}});
const OZe=()=>({}),_Re=()=>undefined,pRe=()=>undefined,bRe=()=>{};
const Cle=()=>false,d=String,U=(rows,predicate)=>rows.filter(predicate).length;
const ey=rows=>rows.findLast(row=>row.type==='assistant');
const $X=row=>row.reason==='prompt_too_long',JTe=row=>row.tokenGap,skn=()=>false;
const tEe=row=>row.reason==='media_too_large',T1=row=>row.detail;
const Yte=text=>({content:text}),hyr=row=>row;
const xZe="Failed to generate conversation summary - the model's reply has no <summary> block";
const textOf=row=>typeof row.message.content==='string'?row.message.content:row.message.content.map(block=>block.text??block.content??'').join('');
const snapshot=text=>text.length>=4096&&text.split(text[0]).join('')===''?{text:text[0],repeat:text.length}:text;
let requests=[],responses=[];
async function $A(request){
 requests.push(request.cacheSafeParams.forkContextMessages.map(row=>snapshot(textOf(row))));
 const reply=responses.shift()??{summary:'<summary>oracle summary</summary>'};
 const success=reply.summary!==undefined;
 const row={type:'assistant',reason:reply.reason,tokenGap:reply.tokenGap,isApiErrorMessage:!success,error:reply.errorKind??'test-error',detail:'test-error',message:{content:[{type:'text',text:reply.summary??'test-error'}],stop_reason:'end_turn'}};
 return {messages:[row],totalUsage:{output_tokens:1}};
}
async function run(testCase){
 requests=[];responses=testCase.responses.map(reply=>typeof reply==='string'?{reason:reply}:reply);
 const rows=testCase.messages.map((row,index)=>({type:row.role,isMeta:row.meta,message:{id:String(index),content:[{type:row.toolResult?'tool_result':'text',...(row.toolResult?{content:row.text.repeat(row.repeat??1)}:{text:row.text.repeat(row.repeat??1)})}]}}));
 const result=await yRe(rows,{systemPrompt:[],toolUseContext:{abortController:{signal:{aborted:false}},options:{tools:[]}}},{allowFallback:testCase.allowFallback,...testCase.initialTokenGap===undefined?{}:{promptTooLong:{gap:testCase.initialTokenGap,actual:Mh(rows)}}});
 return {requests,ok:result.ok,...result.ok?{kept:result.result.messagesToPreserve.map(row=>snapshot(textOf(row))),splitKind:result.result.splitKind,headTruncations:result.result.headTruncations}:{reason:result.reason}};
}
({run});`;
const oracle = vm.runInNewContext(
  promptSource + "\n" + source + "\n" + harness,
  Object.create(null),
  { timeout: 1000 },
);
const previous = JSON.parse(
  readFileSync(
    fileURLToPath(
      new URL(
        "../../crates/compaction/tests/fixtures/claude_2_1_286_compact_fallback.json",
        import.meta.url,
      ),
    ),
    "utf8",
  ),
);
previous.cases.push({
  name: "initial-gap-includes-summary-prompt",
  messages: [
    { role: "user", text: "oldest" },
    { role: "assistant", text: "a1" },
    { role: "user", text: "q2" },
    { role: "assistant", text: "a2" },
    { role: "user", text: "q3" },
    { role: "assistant", text: "a3" },
    { role: "user", text: "q4" },
    { role: "assistant", text: "a4" },
  ],
  responses: [],
  initialTokenGap: 1,
});
const cases = [];
for (const { expected, ...testCase } of previous.cases)
  cases.push({ ...testCase, expected: await oracle.run(testCase) });
const fixture = {
  version: "2.1.318",
  sha256,
  ranges,
  scope:
    "Plain-text split, PTL, opening-round and summary-reask traces. Synthetic model and UTF-16 token estimate; default feature flags, no experimental omission, no media/attachments or provider/UI.",
  cases,
};
const target = fileURLToPath(
  new URL(
    "../../crates/compaction/tests/fixtures/claude_2_1_318_compact_fallback.json",
    import.meta.url,
  ),
);
const output = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output)
    throw Error("Fallback traces differ");
  console.log("Claude Code 2.1.318 fallback oracle matches " + sha256);
} else {
  writeFileSync(target, output);
  console.log(target);
}
