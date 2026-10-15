#!/usr/bin/env node
// Run the hash-pinned native header selector and per-query recovery closure.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = digest(binary);
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges = [];
function take(base, startMarker, endMarker, label) {
  const start = binary.indexOf(startMarker, base);
  const end = binary.indexOf(endMarker, start + startMarker.length);
  assert(start >= base && end > start, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256:digest(bytes)});
  return bytes.toString('utf8');
}
const selector = take(185762130, 'function XMe(e){', 'function sjt(e)', 'XMe header selector');
const closure = take(185762130, 'Lj=!1,Ah=null,m5=!1,hYe=', ',YL=!1', 'hYe/yYe per-query recovery');
const decline = take(180462431, 'function q(e){return e instanceof xt&&e.status===503', 'function he(e)', 'q/gtr/htr headerless decline and wait');
const delay = take(180462431, 'function sL(e,r,n=32000', 'import{STATUS_CODES', 'sL native rounded backoff');
const backoff = take(177883320, 'function pC({attempt:', 'class Mir', 'pC proportional jitter');
const errorClasses = take(177885670, 'class Dd extends xt', 'class yKt', 'native abort/connection/timeout error classes');
const cases = [];
for (const firstParty of [false,true]) for (const auxiliary of [false,true]) {
  for (const dreamy of [false,true]) for (const cedar of [false,true]) {
    for (const kind of ['connection','timeout','aborted','http']) {
      for (const status of kind!=='http'?[null]:[400,403,408,409,429,500,503,529,599]) {
        for (const declined of status===503?[false,true]:[false]) {
          const input = {firstParty,auxiliary,dreamy,cedar,kind,status,declined};
          const trace=[];
          const native = vm.runInNewContext(`(async()=>{
            const config=INPUT, Dn=Object.create(null), Em='anthropic-dispatch-id', WLo='v2s', GLo='v2d', Zy='v2p';
            const ja=()=>config.firstParty, Ss=source=>source, k=name=>name==='tengu_dreamy_frost'?config.dreamy:config.cedar;
            const c=x=>x,S=x=>x,Et=x=>x,rs=x=>x,ye=x=>x,Er=x=>x,t=()=>{},i=(name,metadata)=>TRACE.push({name,metadata});
            const z={model:'fixture-model',querySource:config.auxiliary?'auxiliary':'main'},g=null,K={};
            const ee=async ms=>TRACE.push({sleepMs:ms});
            class xt extends Error { constructor(status,_body,message){super(message??'fixture');this.status=status;this.requestID=status===undefined?undefined:'fixture-request';this.headers={get:()=>config.declined?'false':null};} }
            ${errorClasses}
            Math.random=()=>0.5;
            ${backoff}${delay}${decline}${selector}
            const pe=500;let ${closure};
            const first={};hYe(first,false);
            const error=config.kind==='connection'?new Jd({}):config.kind==='timeout'?new RT():config.kind==='aborted'?new Dd():new xt(config.status);
            const step=await yYe(error), retry={};hYe(retry,false);
            const repeated=await yYe(error), again={};hYe(again,false);
            return {first:first[Em]??null,retry:retry[Em]??null,again:again[Em]??null,
              recovered:step!==undefined,repeated:repeated??null,resending:Lj,trace:TRACE};
          })()`, {INPUT:input,TRACE:trace});
          cases.push({input,expected:JSON.parse(JSON.stringify(await native))});
        }
      }
    }
  }
}
assert.equal(cases.length,208);
const bodyPredicate = take(185762130, 'let ph=Ah!==null&&!Lj&&ld!==null&&!si', ',wf=DUt', 'body-phase eligibility');
const bodyCases=[];
for(const carried of [false,true]) for(const resending of [false,true]) {
  for(const connection of [false,true]) for(const anyEvent of [false,true]) {
    const input={carried,resending,connection,anyEvent};
    const expected=vm.runInNewContext(`(()=>{const Ah=INPUT.carried?'v2d':null,Lj=INPUT.resending,
      ld=INPUT.connection?{code:'ECONNRESET'}:null,si=INPUT.anyEvent;${bodyPredicate};return ph;})()`,{INPUT:input});
    bodyCases.push({input,expected});
  }
}
const delayCases=[0,0.004,0.1,0.5,0.996,0.999999].map(random=>{
  const expected=vm.runInNewContext(`(()=>{${backoff}${delay}const pe=500;return sL(1,undefined,undefined,()=>RANDOM);})()`,{RANDOM:random});
  return {random,expected};
});
const output={version:'2.1.287',native_sha256:nativeSha256,sourceByteRanges,
  scope:'Exact XMe, hYe/yYe, q/gtr/htr, sL/pC and body-phase eligibility. First-party route and query category are supplied inputs; no SDK retries, side-query 4xx budget, post-event stream policy or whole CLI claim.',
  cases,bodyCases,delayCases};
const file=fileURLToPath(new URL('../../crates/llm-runtime/tests/fixtures/dispatch_recovery_2_1_287.json',import.meta.url));
const json=JSON.stringify(output,null,2)+'\n';
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),json);
else writeFileSync(file,json);
process.stdout.write(`${process.argv.includes('--check')?'checked':'wrote'} native dispatch recovery: ${cases.length} failures, ${bodyCases.length} body cases, ${delayCases.length} delays\n`);
