#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const truth=take(177878934,'function Le(e){','function IT(e)','typed bool parser');
const cloud=take(179125203,'function GF(e=He()){','function ja(){','GF/zF provider eligibility');
const envPolicy=take(179726616,'function Yoe(){','function RJe(e)','Yoe/rK/Im environment policy');
const registry=take(179726616,'G_t=R("claude_code",','CR=Object.freeze','beta registry').slice(0,-1);
const order=take(180615120,'var av=[','function nwn()','actual chronological av/lr');
const canonical=take(180615120,'function OI(e){','function xI(e)','OI/Ab built-in canonicalization');
const capabilities=take(180615120,'function rwn(e){','function rN()','rwn/o3r/nN/own capability gates');
const mid=take(180615120,'function oN(e){','function xQn(e)','oN mid-conversation system');
const thinkingCount=take(180615120,'function iN(e){','function wqo(e,n)','iN thinking-token-count gate');
const longContext=take(180615120,'function zd(e){','function VI(e)','zd/n_t context marker');
const assembly=take(180615120,'var aN=[','function awn(e,n)','aN/uN/eZ/vqo model assembly');
const effort=take(181694118,'function jw(e){','function pQ(e)','jw effort support');
const effortInsertion=take(185762130,'function YMe(e,n,r,s,g){','function YLo(e,n,r)','YMe effort beta insertion');
const cases=[];
const envCases=[{}, {CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:'true'}, {CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:'wat'}, {DISABLE_INTERLEAVED_THINKING:'true'}, {DISABLE_INTERLEAVED_THINKING:'wat'}, {DISABLE_INTERLEAVED_THINKING:'\uFEFFon\uFEFF'}, {DISABLE_INTERLEAVED_THINKING:'\u0085on\u0085'}, {CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM:'true'}, {CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM:'false'}, {CLAUDE_CODE_ALWAYS_ENABLE_EFFORT:'true'}, {CLAUDE_CODE_ALWAYS_ENABLE_EFFORT:'wat'}];
for(const provider of ['firstParty','foundry','vertex','bedrock'])for(const model of ['claude-3-5-sonnet','claude-opus-4-0','claude-sonnet-4-0','claude-opus-4-1','claude-sonnet-4-5','claude-haiku-4-5','claude-opus-4-5','claude-opus-4-6','claude-sonnet-4-6','claude-opus-4-7','claude-opus-4-8','claude-sonnet-5','claude-sonnet-5-5','claude-mythos-5','claude-fable-5-1','claude-sonnet-4-6[1m]','claude-opus-4-1-20250805','unknown-model'])for(const env of envCases)for(const interactive of [false,true]){
 const input={provider,model,env,interactive};
 const expected=vm.runInNewContext(`(()=>{
  ${truth}
  const a={...INPUT.env,CLAUDE_CODE_ALWAYS_ENABLE_EFFORT:Le(INPUT.env.CLAUDE_CODE_ALWAYS_ENABLE_EFFORT),CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:Le(INPUT.env.CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS),DISABLE_INTERLEAVED_THINKING:Le(INPUT.env.DISABLE_INTERLEAVED_THINKING),CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM:Le(INPUT.env.CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM)};
  const He=()=>INPUT.provider,cc=()=>He(),ol=()=>false,ft=()=>false,uwn=()=>false,fd=()=>false,Ce=()=>!INPUT.interactive,nwn=()=>false,k=()=>false;
  const gse=()=>undefined,QZ=[],wu=()=>null,qj=()=>false;
  ${canonical}const Be=x=>OI(x.toLowerCase());
  ${cloud}${envPolicy}${order}${capabilities}${mid}${thinkingCount}${longContext}
  const cp='oauth-2025-04-20',R=(name,header)=>({name,header});let ${registry};
  const Dbe=()=>undefined,ny=()=>undefined,QP=()=>false,bkn=()=>false,zQe=()=>undefined,cQn=()=>undefined;
  const fa=(selector,model,compute)=>compute(),p8=oN,K_t=x=>x.toLowerCase().includes('afk-mode'),r4r=x=>({header:x}),Ano=()=>new Set();
  const o4r=new Set([R6t,EU,uEn]),ser=new Set([G_t,R6t,twe,ewe]);
  ${assembly}${effort}${effortInsertion}
  const base=eZ(INPUT.model),body=vqo(INPUT.model),count=base.filter(beta=>ser.has(beta));
  const defaultMain=[...base],mainConfig={};YMe(undefined,mainConfig,{},defaultMain,INPUT.model);
  const explicitMain=[...base],explicitConfig={effort:'low'};YMe(undefined,explicitConfig,{},explicitMain,INPUT.model);
  return {canonical:Be(INPUT.model),structured:own(INPUT.model),effort:jw(INPUT.model),base:base.map(x=>x.header).join(','),body:body.map(x=>x.header),count:count.map(x=>x.header).join(','),default_main:defaultMain.map(x=>x.header).join(','),explicit_main:explicitMain.map(x=>x.header).join(',')};
 })()`,{INPUT:input});cases.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual built-in OI/Ab, av/lr, aN/uN/eZ/vqo, own/jw/YMe with typed Le environment. Served-catalog overrides, dynamic flags, HIPAA, subscribers, beta-denial cache, model aliases/masks, gateway discovery and all awn per-turn features are absent supplied state. No complete native CLI header acceptance.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/llm-runtime/tests/fixtures/betas_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native model beta cases`);
