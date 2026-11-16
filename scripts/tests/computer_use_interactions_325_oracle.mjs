// Exact reviewed 2.1.325 takeover/teach branches with inert host callbacks.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const path=process.argv.slice(2).find(a=>!a.startsWith('--'));
const binary=readFileSync(path);
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const nativeSha256='84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2';
assert.equal(sha(binary),nativeSha256);
const ranges=[
  {
    "name": "p",
    "start": 302154067,
    "end": 302154167,
    "sha256": "4136c18c67daaa1aa4c308a09c7d3c01e6fecaffcea395698c590a0e32bced04"
  },
  {
    "name": "P",
    "start": 302154167,
    "end": 302154220,
    "sha256": "d4781f8482fc2e8392267436c0e7c18f85b560961d6161d5b8b24a50c91e89b5"
  },
  {
    "name": "F",
    "start": 302154220,
    "end": 302154303,
    "sha256": "448b329e776cd7f2df47a580027c6173b2351e9f1f771e2018bbaf5c7aa07522"
  },
  {
    "name": "G",
    "start": 302155308,
    "end": 302155408,
    "sha256": "88edea3e13af43e48dfd8db53054a606980bcfef9a77abddfe43c87d5b319d9d"
  },
  {
    "name": "vo",
    "start": 302188707,
    "end": 302190126,
    "sha256": "e09e0b025173b04c922502016d3931abbeed2c267dafe10a22b694c412af3d57"
  },
  {
    "name": "To",
    "start": 302190126,
    "end": 302190377,
    "sha256": "5e4f891e13e16c7d095eacb193eb7bc86ba2c554ed1d2fb86e30e7eab1f257df"
  },
  {
    "name": "on",
    "start": 302234530,
    "end": 302235612,
    "sha256": "f86e09ee4f03ee18b740c5c7e5b91d0e3a865a57acc1de0a16d4fdeab1bf485f"
  },
  {
    "name": "rn",
    "start": 302235612,
    "end": 302236699,
    "sha256": "3ee434b18171f610e353aa0075d93b4a8daa7da2e6be2718dd8aecd9e751eb46"
  },
  {
    "name": "sn",
    "start": 302236699,
    "end": 302236865,
    "sha256": "4958beb45c093bbe687a481ed10d2e23fa95e5ab4252a442476130acaaf9916d"
  },
  {
    "name": "Wo",
    "start": 302236865,
    "end": 302237388,
    "sha256": "62e3d22db282cb7f44db8d7af075081ef375b6d1db67addf3cddc4cf00189868"
  },
  {
    "name": "Go",
    "start": 302237388,
    "end": 302238254,
    "sha256": "a81695dbae81a2b29d1dddd7ae47e96d71e1fd9fa9bd63f612200bf23122f0ce"
  }
];
const source=ranges.map(r=>{const b=binary.subarray(r.start,r.end);assert.equal(sha(b),r.sha256);return b.toString('utf8');}).join('\n');
const context=vm.createContext({},{codeGeneration:{strings:false,wasm:false}});
vm.runInContext(source+`
var Bt=new Set(['key','type','mouse_move','left_click','left_click_drag','right_click','middle_click','double_click','triple_click','scroll','hold_key','screenshot','cursor_position','left_mouse_down','left_mouse_up','wait']);
async function pe(){} async function me(){}
async function lt(){throw Error('effect seam was not expected');}
async function an(){throw Error('capture seam was not expected');}
`,context,{timeout:1000});
const cases=[];
for(const action of ['request_full_control','release_full_control']) for(const decision of ['allow','deny']) for(const approved of [false,true]) for(const scheduled of [false,true])cases.push({action,input:{},decision,approved,scheduled,active:false});
const malformed=[{}, {explanation:3}, {explanation:'step'}, {explanation:'step',next_preview:'next',actions:null}, {explanation:'step',next_preview:'next',actions:[null]}, {explanation:'step',next_preview:'next',actions:[{}]}, {explanation:'step',next_preview:'next',actions:[{action:'zoom'}]}, {explanation:'step',next_preview:'next',actions:[],anchor:[1]}, {explanation:'step',next_preview:'next',actions:[],anchor:['a',2]}];
for(const input of malformed)for(const active of [false,true])cases.push({action:'teach_step',input,active,decision:'continue'});
for(const input of [{},{steps:[]},{steps:[null]},{steps:[{}]},{steps:[{explanation:'step',next_preview:'next',actions:[]}]}])for(const active of [false,true])for(const decision of ['continue','exit'])cases.push({action:'teach_batch',input,active,decision});
for(const decision of ['continue','exit'])cases.push({action:'teach_step',input:{explanation:'step',next_preview:'next',actions:[]},active:true,decision});
for(const item of cases){
 vm.runInContext(`globalThis.item=${JSON.stringify(item)};globalThis.trace=[];globalThis.host={
    isTakeoverApproved:item.approved,preferredMode:undefined,isUnattended:item.scheduled,
    onTakeoverRequest:async()=>{trace.push('takeover');return {allowed:item.decision==='allow'}},
    approveTakeover:()=>trace.push('approveTakeover'),releaseCuLock:()=>trace.push('releaseCuLock'),revokeTakeover:()=>trace.push('revokeTakeover'),
    grants:{wantsHideBeforeAction:false},onTeachStep:item.active?async()=>{trace.push('teachStep');return {action:item.decision}}:undefined,
    isAborted:()=>false
 };`,context,{timeout:1000});
 const expr=item.action==='request_full_control'?'vo(host)':item.action==='release_full_control'?'To(host)':item.action==='teach_step'?'Wo({},item.input,host,{})':'Go({},item.input,host,{})';
 item.expected=JSON.parse(JSON.stringify(await vm.runInContext(expr,context,{timeout:1000})));
 item.trace=JSON.parse(JSON.stringify(vm.runInContext('trace',context)));
}
const fixture={version:'2.1.325',nativeSha256,sourceByteRanges:ranges,scope:'Takeover allow/deny/scheduled and teach activation/validation/zero-action steps through exact reviewed functions. No UI, input, capture or timing effects.',cases};
const rendered=JSON.stringify(fixture,null,2)+'\n';
const output=new URL('../../crates/tools/computer-use/fixtures/interactions-2.1.325.json',import.meta.url);
if(process.argv.includes('--write'))writeFileSync(output,rendered);else assert.equal(readFileSync(output,'utf8'),rendered);
console.log(`Interaction 2.1.325 oracle: ${cases.length} cases passed`);
