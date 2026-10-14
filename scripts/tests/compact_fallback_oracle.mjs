// Regenerate the narrow Hre/Qho fallback trace goldens from the official binary.
// Usage: node scripts/tests/compact_fallback_oracle.mjs /path/to/2.1.286 [--check]
// Only function ranges are evaluated. No upstream implementation is vendored.
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
const expectedSha = '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433';
if (sha256 !== expectedSha) throw new Error(`Unexpected oracle SHA-256: ${sha256}`);
const entry = binary.indexOf('async function Hre(e,n,r){let s=x0t(e)');
const chunkStart = binary.lastIndexOf('// @bun @bytecode', entry);
const chunkEnd = binary.indexOf('// @bun @bytecode', entry);
if (entry < 0 || chunkStart < 0 || chunkEnd <= entry) throw new Error('Missing Hre source chunk');
const chunk = binary.subarray(chunkStart, chunkEnd);
const declarations = [
  ['var Jxt=', 'function kq('],
  ['function A0(', 'function x0t('],
  ['function rAt(', 'function Kho('],
  ['function Kho(', 'function sAt('],
  ['function sAt(', 'function Vho('],
  ['function Vho(', 'function iAt('],
  ['function iAt(', 'function bIe('],
  ['function bIe(', 'function nAt('],
  ['function nAt(', 'function Xho('],
  ['function Xho(', 'function Qho('],
  ['function Qho(', 'function oAt('],
  ['function oAt(', 'function $re('],
  ['function $re(', 'async function Hre('],
  ['async function Hre(', 'var Jho='],
  ['function Bre(', 'var nse='],
];
const sourceByteRanges = [];
const source = declarations.map(([signature, next]) => {
  const start = chunk.indexOf(signature);
  const end = chunk.indexOf(next, start);
  if (start < 0 || end < start || end - start > 30_000) throw new Error(`Missing oracle range: ${signature}`);
  sourceByteRanges.push({ signature, start: chunkStart + start, end: chunkStart + end });
  return chunk.subarray(start, end).toString('utf8');
}).join('\n');

// Narrow dependencies: plain text estimates use Fm's UTF-16 round-half-up;
// request/model/telemetry hooks are deterministic stubs. Hre, Qho, A0 and Bre
// themselves come directly from the hash-pinned binary above.
const harness = `
var wIe=5,TIe=5000,RIe=25000,bq=3,EAt='[earlier conversation truncated for compaction retry]';
const sentinels=new Set(['[Request interrupted by user]','[Request interrupted by user for tool use]','No response requested.']);
const fD=e=>Array.isArray(e.message.content)&&sentinels.has(e.message.content[0]?.text);
const KC=e=>e.type==='user'&&Array.isArray(e.message.content)&&e.message.content.some(b=>b.type==='tool_result');
const Fm=rows=>rows.reduce((n,r)=>n+(typeof r.message.content==='string'?Math.floor((r.message.content.length+2)/4):r.message.content.reduce((n,b)=>n+Math.floor(((b.text??b.content??'').length+2)/4),0)),0);
const t=()=>{},i=()=>{},me=x=>x,c=x=>x,_=x=>x,BI=()=>false,x0t=A0,wq=()=>{},DI=()=>{},m3e=()=>{},Amt=()=>true,fge=()=>false,Wre=x=>x;
const Re=({content,isMeta})=>({type:'user',isMeta,message:{content}});
const textOf=row=>typeof row.message.content==='string'?row.message.content:row.message.content.map(block=>block.text??block.content??'').join('');
const snapshot=text=>text.length>=4096&&text.split(text[0]).join('')===''?{text:text[0],repeat:text.length}:text;
const success=summaryText=>({ok:true,summaryText,messages:[],totalUsage:{},forkAssistantMessageCount:1});
let requests=[],responses=[];
async function qho(rows){
  requests.push(rows.map(row=>snapshot(textOf(row))));
  return responses.shift()??success('<summary>oracle summary</summary>');
}
async function run(testCase){
  requests=[];
  responses=testCase.responses.map(reply=>typeof reply==='string'?{ok:false,reason:reply}:reply.summary!==undefined?success(reply.summary):{ok:false,...reply});
  const rows=testCase.messages.map((row,index)=>({type:row.role,isMeta:row.meta,message:{id:String(index),content:[{type:row.toolResult?'tool_result':'text',...(row.toolResult?{content:row.text.repeat(row.repeat??1)}:{text:row.text.repeat(row.repeat??1)})}]}}));
  const result=await Hre(rows,{toolUseContext:{abortController:{signal:{aborted:false}}}},{allowFallback:testCase.allowFallback,initialTokenGap:testCase.initialTokenGap});
  return {requests,ok:result.ok,...result.ok?{
    kept:result.result.messagesToPreserve.map(row=>snapshot(textOf(row))),
    splitKind:result.result.splitKind,headTruncations:result.result.headTruncations,
  }:{reason:result.reason}};
}
({run});`;
const oracle = vm.runInNewContext(`${source}\n${harness}`, Object.create(null), { timeout: 1_000 });
const user = text => ({ role: 'user', text });
const assistant = text => ({ role: 'assistant', text });
const largeOpening = [{ ...user('x'), repeat: 400_000 }, { ...assistant('a'), repeat: 80_000 }];
const overflow = { reason: 'prompt_too_long', tokenGap: 10_000 };
const fourRounds = [user('oldest'), assistant('a1'), user('q2'), assistant('a2'), user('q3'), assistant('a3'), user('q4'), assistant('a4')];
const cases = [
  { name: 'one-exchange', messages: [user('q1'), assistant('only reply')], responses: [] },
  { name: 'pending-last-user', messages: [user('q1'), assistant('answer'), user('queued one'), user('queued two'), { ...user('meta context'), meta: true }], responses: [] },
  { name: 'round-ladder-exhausted', messages: [user('oldest'), assistant('a1'), user('q2'), assistant('a2')], responses: ['prompt_too_long'] },
  { name: 'synthetic-marker-retry-bound', messages: [user('q1'), assistant('a1')], responses: ['prompt_too_long', 'prompt_too_long'] },
  { name: 'empty', messages: [], responses: [] },
  { name: 'unanswered', messages: [user('unanswered')], responses: [] },
  { name: 'explicit-opt-out', messages: [user('q1'), assistant('a1')], responses: [], allowFallback: false },
  { name: 'interruption-only', messages: [user('q1'), assistant('[Request interrupted by user]')], responses: [] },
  { name: 'pending-user-cannot-validate-fresh-summary', messages: [user('q'), assistant('[Request interrupted by user]'), { ...user('x'), repeat: 5_000 }], responses: [] },
  { name: 'opening-round-before-initial-head-drop', messages: largeOpening, initialTokenGap: 10_000, responses: [] },
  { name: 'opening-round-after-full-overflow', messages: largeOpening, responses: [overflow] },
  { name: 'opening-round-untagged-response', messages: largeOpening, responses: [overflow, { summary: 'plain response without a summary block' }] },
  { name: 'opening-round-analysis-only-summary', messages: largeOpening, responses: [overflow, { summary: '<analysis><summary>not a real summary</summary></analysis>' }] },
  { name: 'opening-round-error', messages: largeOpening, responses: [overflow, { reason: 'error', errorKind: 'test-error' }] },
  { name: 'opening-round-repeated-media-error', messages: largeOpening, responses: [overflow, 'media_too_large', 'media_too_large'] },
  { name: 'opening-round-second-overflow', messages: largeOpening, responses: [overflow, { reason: 'prompt_too_long', tokenGap: 4_000 }] },
  { name: 'opening-round-threshold-inclusive', messages: largeOpening, initialTokenGap: 20_000, responses: [] },
  { name: 'opening-round-threshold-exceeded', messages: largeOpening, initialTokenGap: 20_001, responses: [] },
  { name: 'latest-round-gap-replaces-old-gap', messages: fourRounds, responses: [{ reason: 'prompt_too_long', tokenGap: 1_000 }, { reason: 'prompt_too_long', tokenGap: 1 }] },
  { name: 'unknown-round-gap-replaces-old-gap', messages: fourRounds, responses: [{ reason: 'prompt_too_long', tokenGap: 1_000 }, 'prompt_too_long'] },
];
const fixture = {
  version: '2.1.286', sha256, sourceByteRanges,
  scope: 'Hre/Qho/nAt/Xho request-history, opening-round rescue and preserved-tail traces with a stub summarizer; excludes model prompts, virtual/API-error metadata, provider traffic and UI. Repeated plain-text fixtures use lossless text/repeat descriptors.',
  cases: [],
};
for (const testCase of cases) fixture.cases.push({ ...testCase, expected: await oracle.run(testCase) });
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const path = fileURLToPath(new URL('../../crates/compaction/tests/fixtures/claude_2_1_286_compact_fallback.json', import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(path, 'utf8') !== output) throw new Error('Compact fallback oracle fixture differs');
  console.log(`Compact fallback traces match Claude Code ${fixture.version} (${sha256})`);
} else {
  writeFileSync(path, output);
  console.log(path);
}
