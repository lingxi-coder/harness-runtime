// Replay hash-pinned 2.1.319 compact split/retry functions with a synthetic model.
// Runtime flags use artifact defaults; the experimental tool-result omission flag is off.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (
  sha256 !== "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd"
)
  throw Error("Unexpected baseline");
const ranges = [
  {
    "name": "g6",
    "offset": 271565319,
    "bytes": 267
  },
  {
    "name": "ROt",
    "offset": 271565586,
    "bytes": 74
  },
  {
    "name": "Qhn",
    "offset": 271583884,
    "bytes": 567
  },
  {
    "name": "Qxe",
    "offset": 271587607,
    "bytes": 236
  },
  {
    "name": "ePe",
    "offset": 271587843,
    "bytes": 93
  },
  {
    "name": "syn",
    "offset": 271588966,
    "bytes": 3455
  },
  {
    "name": "aot",
    "offset": 271592421,
    "bytes": 135
  },
  {
    "name": "kbr",
    "offset": 271592556,
    "bytes": 77
  },
  {
    "name": "Sbr",
    "offset": 271592633,
    "bytes": 114
  },
  {
    "name": "iyn",
    "offset": 271592747,
    "bytes": 38
  },
  {
    "name": "oPe",
    "offset": 271592798,
    "bytes": 144
  },
  {
    "name": "lot",
    "offset": 271592942,
    "bytes": 59
  },
  {
    "name": "tyn",
    "offset": 271593001,
    "bytes": 144
  },
  {
    "name": "bbr",
    "offset": 271593145,
    "bytes": 281
  },
  {
    "name": "Tbr",
    "offset": 271593426,
    "bytes": 153
  },
  {
    "name": "nyn",
    "offset": 271593579,
    "bytes": 217
  },
  {
    "name": "iot",
    "offset": 271593796,
    "bytes": 659
  },
  {
    "name": "ayn",
    "offset": 271594455,
    "bytes": 149
  },
  {
    "name": "Ebr",
    "offset": 271594604,
    "bytes": 241
  },
  {
    "name": "oyn",
    "offset": 271594859,
    "bytes": 51
  },
  {
    "name": "ryn",
    "offset": 271594910,
    "bytes": 56
  },
  {
    "name": "nPe",
    "offset": 271594966,
    "bytes": 96
  },
  {
    "name": "rPe",
    "offset": 271595062,
    "bytes": 7864
  },
  {
    "name": "ore",
    "offset": 271602926,
    "bytes": 115
  },
  {
    "name": "sre",
    "offset": 271908810,
    "bytes": 407
  },
  {
    "name": "pot",
    "offset": 271911816,
    "bytes": 84
  },
  {
    "name": "ire",
    "offset": 271911900,
    "bytes": 119
  },
  {
    "name": "sPe",
    "offset": 271912019,
    "bytes": 84
  },
  {
    "name": "cWe",
    "offset": 274520856,
    "bytes": 184
  }
];
const source = ranges
  .map(({ offset, bytes }) =>
    binary.subarray(offset, offset + bytes).toString("utf8"),
  )
  .join("\n");
const promptStart = binary.lastIndexOf('var eot="an <analysis>');
const promptEnd = binary.indexOf("function yki(", promptStart);
const promptSource = binary.subarray(promptStart, promptEnd).toString("utf8");
const harness = String.raw`
const hPt="test-timeout",$p="artifact",bv="remote-control",rre=3,BAr=10000,dot=5,uot=5000,cot=25000,wbr=1000,A5=[],bxt=new Set;
const Xxe={ids:new Set,madeByToolName:new Map,refused:0,withMedia:0,textLength:0};
const t=()=>{},a=()=>{},l=()=>{},m=()=>{},de=x=>x,f=x=>x,S=x=>x;
const w_r=()=>true,Vfs=()=>false,zMi=()=>true,VMi=()=>false,le=()=>false;
const vbr=15000,U3=()=>false;
const FMi=()=>false,$Mi=()=>false,Tpo=async()=>{},Hd=()=>{},c=()=>{},g=()=>{};
const are=()=>undefined,WW=()=>undefined,G7=()=>[],Jhn=()=>undefined,nre=()=>undefined,lPe=()=>undefined;
const vs=x=>x,BS=()=>false,EN=row=>['[Request interrupted by user]','[Request interrupted by user for tool use]','No response requested.'].includes(textOf(row));
const Ch=row=>Array.isArray(row.message.content)&&row.message.content.some(block=>block.type==='tool_result');
const gc=text=>Math.floor((text.length+2)/4);
const Bh=rows=>rows.reduce((total,row)=>total+gc(textOf(row)),0);
const UW=x=>x,rce=x=>x,got=x=>x;
const krt='[earlier conversation truncated for compaction retry]';
const Ie=({content,isMeta})=>({type:'user',isMeta,message:{content}});
const mot=()=>({}),iPe=()=>undefined,tPe=()=>undefined,aPe=()=>{};
const xde=()=>false,d=String,U=(rows,predicate)=>rows.filter(predicate).length;
const jh=rows=>rows.findLast(row=>row.type==='assistant');
const x7=row=>row.reason==='prompt_too_long',QCe=row=>row.tokenGap,WTn=()=>false;
const oRe=row=>row.reason==='media_too_large',O$=row=>row.detail;
const tre=text=>({content:text}),zbr=row=>row;
const fot="Failed to generate conversation summary - the model's reply has no <summary> block";
const textOf=row=>typeof row.message.content==='string'?row.message.content:row.message.content.map(block=>block.text??block.content??'').join('');
const snapshot=text=>text.length>=4096&&text.split(text[0]).join('')===''?{text:text[0],repeat:text.length}:text;
let requests=[],responses=[];
async function iR(request){
 requests.push(request.cacheSafeParams.forkContextMessages.map(row=>snapshot(textOf(row))));
 const reply=responses.shift()??{summary:'<summary>oracle summary</summary>'};
 const success=reply.summary!==undefined;
 const row={type:'assistant',reason:reply.reason,tokenGap:reply.tokenGap,isApiErrorMessage:!success,error:reply.errorKind??'test-error',detail:'test-error',message:{content:[{type:'text',text:reply.summary??'test-error'}],stop_reason:'end_turn'}};
 return {messages:[row],totalUsage:{output_tokens:1}};
}
async function run(testCase){
 requests=[];responses=testCase.responses.map(reply=>typeof reply==='string'?{reason:reply}:reply);
 const rows=testCase.messages.map((row,index)=>({type:row.role,isMeta:row.meta,message:{id:String(index),content:[{type:row.toolResult?'tool_result':'text',...(row.toolResult?{content:row.text.repeat(row.repeat??1)}:{text:row.text.repeat(row.repeat??1)})}]}}));
 const result=await rPe(rows,{systemPrompt:[],toolUseContext:{abortController:{signal:{aborted:false}},options:{tools:[]}}},{allowFallback:testCase.allowFallback,...testCase.initialTokenGap===undefined?{}:{promptTooLong:{gap:testCase.initialTokenGap,actual:Bh(rows)}}});
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
        "../../crates/compaction/tests/fixtures/claude_2_1_318_compact_fallback.json",
        import.meta.url,
      ),
    ),
    "utf8",
  ),
);

const cases = [];
for (const { expected, ...testCase } of previous.cases)
  cases.push({ ...testCase, expected: await oracle.run(testCase) });
const fixture = {
  version: "2.1.319",
  sha256,
  ranges,
  scope:
    "Plain-text split, PTL, opening-round and summary-reask traces. Synthetic model and UTF-16 token estimate; default feature flags, no experimental omission, no media/attachments or provider/UI.",
  cases,
};
const target = fileURLToPath(
  new URL(
    "../../crates/compaction/tests/fixtures/claude_2_1_319_compact_fallback.json",
    import.meta.url,
  ),
);
const output = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output)
    throw Error("Fallback traces differ");
  console.log("Claude Code 2.1.319 fallback oracle matches " + sha256);
} else {
  writeFileSync(target, output);
  console.log(target);
}
