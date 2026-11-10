#!/usr/bin/env node
// Evaluate the shipped 2.1.319 scheduler, not a handwritten JS reimplementation.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: node auto_dream_319_oracle.mjs <official 2.1.319 binary> [--check]');
const bytes = readFileSync(process.argv[2]);
const sha256 = data => createHash('sha256').update(data).digest('hex');
const nativeSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha256(bytes), nativeSha256);
const ranges = [];
function between(first, last, offset, label) {
  const start = bytes.indexOf(first, offset);
  const endExclusive = bytes.indexOf(last, start + first.length);
  assert(start >= offset && endExclusive > start, label);
  const body = bytes.subarray(start, endExclusive);
  ranges.push({ label, start, endExclusive, sha256: sha256(body) });
  return body.toString('utf8');
}
const featureSource = between('function K(){return k("tengu_onyx_plover",null)}', 'class Z{', 278849000, 'availability and settings gate');
const schedulerSource = between('var w=null,ce=600000,pe=', 'function ye(', 278856000, 'thresholds, host gates and automatic scheduler');
const lockSource = between('var g=".consolidate-lock",y=3600000;', 'async function EVt(', 278539000, 'local PID lock reader and acquisition');
const rollbackSource = between('async function EVt(', 'async function S(', 278541000, 'local consolidation timestamp rollback');
const sessionIdSource = between('var le=/^[0-9a-f]{8}', 'function z(', 260491000, 'session UUID filename validator');
const sessionListSource = between('async function Ejo(', 'var h=32;', 277105000, 'local session catalog scan');
const sessionIdCases=['00000000-0000-0000-0000-000000000001','ABCDEFAB-1234-5678-ABCD-0123456789AB','agent-00000000-0000-0000-0000-000000000001','newer','00000000-0000-0000-0000-00000000000g', '00000000-0000-0000-0000-000000000001\n'].map(input=>{
  const c=vm.createContext({});vm.runInContext(sessionIdSource,c);
  return {input,expected:vm.runInContext(`e1(${JSON.stringify(input)})`,c)};
});
const scanInput={since_ms:1000.25,files:[
  {name:'00000000-0000-0000-0000-000000000001.jsonl',mtime_ms:1000},
  {name:'00000000-0000-0000-0000-000000000002.jsonl',mtime_ms:1001},
  {name:'not-a-session.jsonl',mtime_ms:2000},
  {name:'00000000-0000-0000-0000-000000000003.JSONL',mtime_ms:2000},
  {name:'00000000-0000-0000-0000-000000000004.jsonl',mtime_ms:null},
]};
const scanContext=vm.createContext({X:()=>false,P:async()=>scanInput.files.map(file=>file.name),g:(...parts)=>parts.join('/'),w:async path=>{
  const row=scanInput.files.find(file=>path.endsWith('/'+file.name));if(row?.mtime_ms===null)throw new Error('missing');return{mtime:{getTime:()=>row.mtime_ms}};
}});
vm.runInContext(sessionIdSource+sessionListSource,scanContext);
const scanResult=await vm.runInContext('Ejo("/transcripts",true)',scanContext);
const scanCase={input:scanInput,expected:scanResult.filter(row=>row.mtime>scanInput.since_ms).map(row=>row.sessionId)};

const gateCases = [];
for (const flag of [null, { enabled: true }, { available: true }, { enabled: false, available: false }, { enabled: 'true', available: 1 }]) {
  for (const setting of [null, false, true]) {
    const gates = { server_enabled: flag?.enabled === true, server_available: flag?.available === true, setting, auto_memory_enabled: true, is_subagent: false, is_sdk: false, memory_read_only: false };
    const c = vm.createContext({ k: () => flag, yt: () => setting === null ? {} : { autoDreamEnabled: setting } });
    vm.runInContext(featureSource, c);
    gateCases.push({ flag, gates, expected: vm.runInContext('Tvt()', c) });
  }
}
for (const override of [{is_subagent:true}, {is_sdk:true}, {memory_read_only:true}, {auto_memory_enabled:false}]) {
  const gates = { server_enabled: true, server_available: false, setting: null, auto_memory_enabled: true, is_subagent: false, is_sdk: false, memory_read_only: false, ...override };
  const c = vm.createContext({ k: () => ({enabled:true}), yt: () => ({}), rr: () => gates.is_subagent ? 'agent' : null, h4: () => gates.is_sdk, fh: () => gates.auto_memory_enabled, xue: () => gates.memory_read_only, qGr: 'rm' });
  vm.runInContext(featureSource + schedulerSource, c);
  gateCases.push({ flag: {enabled:true}, gates, expected: vm.runInContext('he()', c) });
}

const thresholdCases = [null, {}, {minHours:0,minSessions:0}, {minHours:-1,minSessions:-2}, {minHours:'1',minSessions:'2'}, {minHours:0.5,minSessions:1.5}, {minHours:48,minSessions:12}].map(flag => {
  const c = vm.createContext({ k: () => flag, qGr: 'rm' });
  vm.runInContext(schedulerSource, c);
  const result = vm.runInContext('fe()', c);
  return { flag, expected: { min_hours:result.minHours, min_sessions:result.minSessions } };
});

async function runScenario(input) {
  const host = {};
  const trace = [];
  let now = input.now_ms;
  let settings = { ...(input.flag ?? {enabled:true}) };
  const c = vm.createContext({
    Date: {now: () => now},
    qGr:'rm', B:{of: () => host}, k: () => settings, yt: () => ({}),
    rr:() => null, h4:() => false, fh:() => true, xue:() => false,
    sCt:async () => {trace.push('read_lock'); return input.prior_ms;},
    TZs:async () => {trace.push('scan_sessions'); return input.sessions;}, G:()=>'current',
    YGr:()=>input.writes_denied?'write-rule':undefined,w7:()=>({}),dYn:()=> 'rule',
    KGr:()=>undefined,
    CZs:async()=>{trace.push('acquire_lock');return input.lock_held?null:input.prior_ms;},
    EVt:async()=>{trace.push('rollback');},
    El:()=>'/memory', bse:()=>false, t:()=>{}, l:()=>{}, S:x=>x, f:x=>x,
    ee:()=>{trace.push('register');return'dream-1';},
    Hy:()=>'/transcripts',ve:()=>'/repo',Q:()=>'',Cvr:()=>false,_e:()=>()=>{},
    iR:async()=>{trace.push('fork');if(input.fork_fails)throw new Error('provider');return{totalUsage:{cache_read_input_tokens:0,cache_creation_input_tokens:0,output_tokens:0}};},
    lZ:()=>({}),XGr:()=>()=>{},$8t:()=>false,Ie:content=>content,
    re:()=>{trace.push('complete');},oe:()=>{trace.push('fail');},Y:e=>e?.type==='dream',
    P:(n,s)=>n===1?s:s+'s',d:e=>String(e),N:e=>e,xG:()=> 'Error',m:()=>{},
    AbortController,
  });
  vm.runInContext(featureSource+schedulerSource,c);
  vm.runInContext('IXs({})',c);
  const session={toolUseContext:{getAppState:()=>({toolPermissionContext:{}}),taskRegistry:{get:()=>({type:'dream',filesTouched:[]})}}};
  await host.runner(session);
  const first = [...trace];
  if(input.second_call_delta_ms!==undefined){ now+=input.second_call_delta_ms;trace.length=0;await host.runner(session); }
  return {input,expected:first,...input.second_call_delta_ms!==undefined?{second_expected:[...trace]}:{}};
}
const now = 10*24*3600000;
const defaultInput = {now_ms:now,prior_ms:now-24*3600000,sessions:['current','s1','s2','s3','s4','s5']};
const runnerCases=[];
for(const patch of [
  {},{prior_ms:now-24*3600000+1},{sessions:['current','s1','s2','s3','s4']},
  {writes_denied:true},{lock_held:true},{fork_fails:true},
  {flag:{enabled:true,minHours:1,minSessions:1.5},sessions:['current','s1']},
  {flag:{enabled:true,minHours:1,minSessions:1.5},sessions:['current','s1','s2']},
  {sessions:['current'],second_call_delta_ms:599999},
  {sessions:['current'],second_call_delta_ms:600000},
]) runnerCases.push(await runScenario({...defaultInput,...patch}));

async function lockScenario(input){
  let text=input.existing_text;
  let mtime=input.existing_mtime;
  const trace=[];
  const c=vm.createContext({
    A:(...parts)=>parts.join('/'),El:()=>'/memory',X:()=>false,
    Date:{now:()=>input.now_ms},process:{pid:42},
    p:async()=>{if(mtime===null)throw new Error('missing');return{mtimeMs:mtime};},
    f:async()=>{if(text===null)throw new Error('missing');return text;},
    P:async()=>{trace.push('mkdir');},
    k:async(_path,value)=>{trace.push('write:'+value);text=input.contender_text??value;mtime=input.now_ms;},
    kl:pid=>input.live_pids.includes(pid),t:()=>{},
  });
  vm.runInContext(lockSource,c);
  const result=await vm.runInContext('L("/memory")',c);
  return{input,expected:{previous_ms:result,trace}};
}
const lockCases=[];
for(const input of [
  {now_ms:now,existing_mtime:null,existing_text:null,live_pids:[]},
  {now_ms:now,existing_mtime:now-3599999,existing_text:'7',live_pids:[7]},
  {now_ms:now,existing_mtime:now-3600000,existing_text:'7',live_pids:[7]},
  {now_ms:now,existing_mtime:now-1,existing_text:'7',live_pids:[]},
  {now_ms:now,existing_mtime:now+1000,existing_text:'  +7 rest',live_pids:[7]},
  {now_ms:now,existing_mtime:null,existing_text:null,live_pids:[],contender_text:'99'},
])lockCases.push(await lockScenario(input));

const rollbackCases=[];
for(const prior_ms of [0,123456789]){
  const trace=[];
  const c=vm.createContext({A:(...parts)=>parts.join('/'),El:()=>'/memory',X:()=>false,
    v:async()=>{trace.push({operation:'unlink'});},k:async(_path,value)=>{trace.push({operation:'write',value});},
    D:async(_path,atime,mtime)=>{trace.push({operation:'utimes',atime,mtime});},t:()=>{},d:String});
  vm.runInContext(lockSource+rollbackSource,c);
  await vm.runInContext(`C('/memory',${prior_ms})`,c);
  rollbackCases.push({prior_ms,expected:trace});
}

const sessions=['a','session space','unicode-猫'];
const schedulerStart=bytes.indexOf('let i=Hy(ve()),z=`',278857000);
const templateStart=bytes.indexOf('`',schedulerStart);
const templateEnd=bytes.indexOf('`,we=',templateStart);
assert(templateEnd>templateStart);
const contextTemplate=bytes.subarray(templateStart,templateEnd+1);
ranges.push({label:'automatic scheduler context template',start:templateStart,endExclusive:templateEnd+1,sha256:sha256(contextTemplate)});
const extra=vm.runInNewContext(contextTemplate.toString('utf8'),{p:sessions});

const output=JSON.stringify({version:'2.1.319',native_sha256:nativeSha256,source_byte_ranges:ranges,session_id_cases:sessionIdCases,session_scan:scanCase,gate_cases:gateCases,threshold_cases:thresholdCases,runner_cases:runnerCases,lock_cases:lockCases,rollback_cases:rollbackCases,additional_context:{sessions,expected:extra}},null,2)+'\n';
const path=new URL('./fixtures/auto_dream_319.json',import.meta.url);
if(process.argv.includes('--check')){
  assert.equal(readFileSync(path,'utf8'),output,'Native auto-dream scheduler oracle drifted');
  console.log(`Claude Code 2.1.319 auto-dream oracle: ${gateCases.length} gates, ${thresholdCases.length} thresholds, ${runnerCases.length} runner, ${lockCases.length} lock and ${rollbackCases.length} rollback scenarios match`);
}else{writeFileSync(path,output);console.log('Wrote auto_dream_319.json');}
