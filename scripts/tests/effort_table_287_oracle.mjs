#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {resolve as pathResolve} from 'node:path';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=bytes=>createHash('sha256').update(bytes).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const persist=take(181694118,'function dBe(e){','function eXn(','native persistable dBe');
const identity=take(181694118,'function qSe(e){','function be(e,n)','qSe');
const table=take(181694118,'function U6r(){','function he(e)','U6r/Q/ve/i_n/ee');
const session=take(181694118,'function he(e){','function HXe(e)','he/qSe/be/kd');
const exemptions=take(181694118,'var Se=new Set(', 'function U6r(){','Se/$6r/ye');
const syntax=take(178300800,'function Ct(e){','export{hse,','wu/qj with native model shapes');
const prototypes=Object.getOwnPropertyNames(Object.prototype);
const cases=[];
const ids={sonnet:'claude-sonnet-4-6',s:'claude-sonnet-4-6','1':'claude-sonnet-4-6','2':'claude-sonnet-4-6',opus:'claude-opus-5-5','__PROTO__':'__proto__','Constructor':'constructor','custom-fable':'claude-opus-4-6'};
const models=['sonnet','claude-sonnet-4-6','opus','claude-opus-5-5','claude-sonnet-5-5','claude-opus-4-7','claude-opus-4-8','claude-fable-5','claude-mythos-5-1','claude-opus-6-20260928','us-gov.anthropic.claude-opus-6-v1:0','vendor/claude-opus-6-latest','claude-opus-6-custom','unknown-model','constructor','__proto__'];
const sessions=[null,{kind:'inherit'},{kind:'default'},{kind:'level',value:'low'},{kind:'level',value:0},{kind:'level',value:null}];
function run(input) {
 const expected=vm.runInNewContext(`(()=>{
  const X={kind:'inherit'},a=INPUT.env??{},le=()=>INPUT.global??{},Z7e=()=>INPUT.override?{name:INPUT.override}:null,QYn=()=>true;
  const Be=e=>Object.hasOwn(INPUT.identities,e)?INPUT.identities[e]:e,Rt=e=>e,ue=PATH_RESOLVE,Xo=()=>INPUT.layers.map(layer=>layer.source),Zn=source=>INPUT.layers.find(layer=>layer.source===source).path,ge=source=>INPUT.layers.find(layer=>layer.source===source).settings;
  const QZ=['us','eu','apac','jp','au','us-gov','global'],Vj=[],zj=[],Xxe=e=>e.startsWith('claude-fable-');
  ${syntax}${persist}${identity}${exemptions}${table}${session}
  const result=Q();
  return {default:result.default??null,user_effort:result.legacyUserEffort??null,by_model:Object.keys(result.byModel).map(key=>[key,result.byModel[key]??null]).sort((a,b)=>a[0]<b[0]?-1:a[0]>b[0]?1:0),excluded:ye(),model_user:INPUT.models.map(model=>{const id=qSe(model);return $6r(id)}),resolved:INPUT.sessions.map(session=>INPUT.models.map(model=>{const result=kd({sessionEffort:session??undefined,settingsEffortTable:resultTable},model);return {present:result!==undefined,value:result??null}}))};
 })()`.replace('const result=Q();','const result=Q(),resultTable=result;'),{INPUT:input,PATH_RESOLVE:path=>pathResolve('/',path)});
 cases.push({input,expected});
}
const maps=[{}, {sonnet:{effortLevel:'low'}}, {sonnet:{effortLevel:'high'},'claude-sonnet-4-6':{effortLevel:'medium'}}, {'claude-sonnet-4-6':{effortLevel:'medium'},sonnet:{effortLevel:'high'}}, {s:{effortLevel:'high'},sonnet:{effortLevel:'low'}}, {'2':{effortLevel:'high'},'1':{effortLevel:'low'}}, {opus:{effortLevel:'xhigh'},sonnet:{effortLevel:'low'}}, {'__PROTO__':{effortLevel:'low'},Constructor:{effortLevel:'high'}}];
for(const user of [undefined,'low','medium','high'])for(const project of [undefined,'medium','xhigh'])for(const flag of [undefined,'high'])for(const entries of maps)for(const override of [null,'sonnet'])for(const global of [{},{firstStartVersion:'2.1.287'},{unpinOpus47LaunchEffort:true,unpinOpus48LaunchEffort:'yes',unpinFable5LaunchEffort:true}]) {
 const layer=(source,effort,modelSettings={})=>({source,settings:{...effort!==undefined&&{effortLevel:effort},modelSettings}});
 const layers=[layer('userSettings',user,entries),layer('projectSettings',project),layer('flagSettings',flag)];
 run({layers,global,override,identities:ids,models,sessions,env:{}});
}
for(const fable of ['custom-fable','claude-fable-5-1','claude-opus-5-5']) run({layers:[{source:'userSettings',settings:{effortLevel:'low'}}],global:{},env:{ANTHROPIC_DEFAULT_FABLE_MODEL:fable},identities:ids,models,sessions});
// Path suppression must happen before source priority, except flagSettings.
for(const same of [false,true]) run({layers:[{source:'userSettings',path:'/settings.json',settings:{effortLevel:'low'}},{source:'projectSettings',path:same?'/x/../settings.json':'/project.json',settings:{effortLevel:'high'}},{source:'flagSettings',path:'/settings.json',settings:{modelSettings:{sonnet:{effortLevel:'medium'}}}}],global:{firstStartVersion:null},env:{},identities:ids,models,sessions});
const syntaxModels=['claude-opus-6','claude-opus-99-99','claude-opus-100','claude-6-opus','claude-6-7-opus','claude-opus-6-v123:456','claude-opus-6-v1234','claude-opus-6-fast-v1@20261002-v12:3','claude-opus-6-latest','claude-opus-6-extra','vendor/claude-opus-6[2m]','xx.anthropic.claude-opus-6-v1:0','au.anthropic.claude-opus-6-v1:0','claude-opus-٦','claude-opus-6-v１２','\uFEFFclaude-opus-6\uFEFF','\u0085claude-opus-6','claude-opus-6\t'];
run({layers:[{source:'userSettings',settings:{effortLevel:'medium'}}],global:{firstStartVersion:'x'},env:{},identities:ids,models:syntaxModels,sessions});
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual U6r/Q/ve/ee/he/kd/dBe/Se/$6r/ye and wu/qj. Admitted source documents, selected canonical aliases and managed override eligibility are supplied. No served-catalog acquisition, native CLI command or live-provider acceptance.',prototypes,cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/core/tests/fixtures/effort_table_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native settings effort tables`);
