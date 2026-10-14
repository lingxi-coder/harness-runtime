#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];
function take(first, last, offset, label, maxLength = 4000) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < maxLength, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}
const main = take('fe=b.startsWith("repl_main_thread")||b==="sdk"', ',ot=fe', 195111000, 'native main query classification', 100);
const scope = take('Je=fe,Is=b==="side_question"||e.forkOrigin==="btw"', ',fo=es(b)', 195114000, 'native session and local notice scope', 100);
const modelIdentity = take('function Mn(e){return e.replace', 'function Gy(', 179326700, 'native model identity', 500);
const receivedModel = take('function Ucn(e,n)', 'function jHt(', 188774000, 'native received model selection', 500);
const accepted = take('let xe=ge,ve=Ucn(ao?.model,M.toModel)', 'let tn=new Set(M.discardedMessages', 195138675, 'native allowlist and unconditional accepted session mutation');

const context = vm.createContext({});
vm.runInContext(`${modelIdentity}${receivedModel}
function scopes(b,e){let fe,Je,Is;${main};${scope};return {isMainThread:Je,emitsLocalScope:Is};}
async function* accept(input){
  let state={mainLoopModel:input.appModel,mainLoopModelForSession:null,fastMode:false};
  let ge=input.queryModel,ao={model:input.declaredModel},M={toModel:input.toModel,reason:input.reason,apiRefusalCategory:input.category,requestId:'request'};
  let X={swapSession:input.swapSession},ro=0,Fe,so,Ge=false,mn,latch,armed=false;
  let uA=()=>true,iqr=()=>({loopEffort:undefined,latchEffort:undefined}),MK=()=>true;
  let jn=model=>{ge=model},Os=()=>true,Evt=()=>{armed=true},Fc=()=>undefined;
  let vvt=value=>{latch=value},Io=()=>false,Yk=()=>false,Wp=()=>{},un=()=>1,Xk=()=>{};
  let f={agentContext:{},options:{},getAppState:()=>state,setAppState:update=>{state=update(state)}};
  do {${accepted}
    return {fromModel:xe,toModel:ve,sameModel:Tt,queryModel:ge,appModel:state.mainLoopModel,
      previousAppModel:latch?.previousAppStateModel??null,armed,count:ro};
  } while(false);
}
`, context);

const scopeCases = [
  ['repl_main_thread', null], ['repl_main_thread:scheduled', null], ['sdk', null],
  ['agent:custom:reviewer', null], ['agent:custom:reviewer', 'btw'],
  ['side_question', null], ['', null], ['', 'btw'], ['SDK', null],
].map(([querySource,forkOrigin])=>({querySource,forkOrigin,expected:context.scopes(querySource,{forkOrigin:forkOrigin??undefined})}));
const acceptedInputs = [
  {queryModel:'A',appModel:'user-pick',declaredModel:'B',toModel:'B',reason:'sticky',category:null,swapSession:true},
  {queryModel:'B',appModel:'user-pick',declaredModel:'C',toModel:'C',reason:'refusal',category:'cyber',swapSession:true},
  {queryModel:'A',appModel:'A',declaredModel:'B',toModel:'B',reason:'sticky',category:'cyber',swapSession:true},
  {queryModel:'A',appModel:'A',declaredModel:'B',toModel:'B',reason:'refusal',category:'cyber',swapSession:false},
  {queryModel:'B[1m]',appModel:'different-app',declaredModel:'B[1m]',toModel:'B',reason:'sticky',category:null,swapSession:true},
];
const acceptedCases = [];
for (const input of acceptedInputs) {
  acceptedCases.push({input,expected:(await context.accept(input).next()).value});
}
assert.equal(acceptedCases[0].expected.appModel,'B');
assert.equal(acceptedCases[0].expected.previousAppModel,'user-pick');
assert.equal(acceptedCases[1].expected.fromModel,'B');
assert.equal(acceptedCases[1].expected.armed,true);
assert.equal(acceptedCases[2].expected.armed,false);
assert.equal(acceptedCases[3].expected.appModel,'A');
assert.equal(acceptedCases[4].expected.sameModel,true);
const data = {
  version:'2.1.288',native_sha256:sha256(binary),sourceByteRanges,
  scope:'Actual native query-source/fork-origin predicates and accepted model mutation. Model policy is supplied as allowed; effort, fast mode, app-state getter/setter, and latch storage are supplied dependencies. This proves acceptance snapshots the current app model without a request-ownership gate, and preserves query-local notice identity. It does not prove production UI live model picking, provider selection, dynamic allowlist, or effort acquisition.',
  scopeCases,acceptedCases,
};
const output=JSON.stringify(data,null,2)+'\n';
const path=new URL('../../crates/orchestrator/tests/fixtures/server_fallback_scope_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),output);
else writeFileSync(path,output);
console.log(JSON.stringify({scopeCases:scopeCases.length,acceptedCases:acceptedCases.length,sourceRanges:sourceByteRanges.length}));
