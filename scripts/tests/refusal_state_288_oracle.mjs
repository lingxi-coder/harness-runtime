#!/usr/bin/env node
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
import {nativeBetaWorld} from './common/anthropic_native_288.mjs';
const {binary,hash,ranges,take,context}=nativeBetaWorld(process.argv[2]);
const selection=take('class pe{','class fe{',178985800,'native model-selection storage');
const latch=take('function vvt(e)','function oae(',179040950,'native latch retarget');
const transition=take('function vn()','var yn=',179032900,'native session transition restoration');
const resume=take('function ie(e)','async function Me(',194897040,'native model restore, fork neutralization and cyber header restore');
const wire=take('function Mn(e){return e.replace','function Gy(',179326700,'native wire model identity');
const canonical=take('function qI(e)','function ',181784550,'native bare-wire family branch');
const date=take('function Bb(e)','function ',181786200,'native date identity suffix');
const resumeModelLoader=take('async function lCe(e,o,r,{scheduleTelemetry:n=(l)=>l(),storageV5:s,credentials:u}={})','function zur(',194895000,'native cold-resume model loader',1000);
const assistantModelChooser=take('function N(e,o){let r=new Set(hJo.map((u)=>Be(u)))','function ie(',194896000,'native cold-resume assistant model selection',1000);
const constants=take('var wI="x-is-refusal-fallback"','function efn(',185910100,'actual refusal header names');
const origin=take('function efn(e)','function RI(',185910000,'native origin header validation');
const header=take('if(w&&(vB()&&RI()||JHe()))','if(cd(D)&&Ka()&&T1e())',185940500,'native refusal headers before fetch');
context.Headers=Headers;
vm.runInContext(`var Mee=['us','eu','apac','global'];function sie(){return undefined;}function Iu(e){return {base:e};}function TW(){return false;}
${canonical}${date}${wire}${selection}
function Be(e){return qI(e.toLowerCase());}function n(){return host;}function Wp(e){host.modelSelection.overrideMainLoopModel(e);}function Fc(){return host.modelSelection.mainLoopModelOverride();}function KL(){return initialModel;}
function ast(){}function zbe(){return false;}function T(){}function Z(){}function Evt(e){host.modelSelection.armRefusalFallbackHeader(e);}function i(){}function d(e){return e;}
${latch}${transition}${resume}${constants}${origin}function Z(){}
function fresh(){return {modelSelection:new pe()};}
function headers(first,armed,occurred,lane,id){var w=first,M=new Headers([['x-is-refusal-fallback','explicit'],['x-cc-fallback-latched-by','prior']]);function vB(){return occurred;}function RI(){return lane;}function JHe(){return armed;}function dro(){return id;}${header}return Object.fromEntries(M);}`,context);
// The bootstrap helper imports are replaced after extraction; routing/flags are
// explicit facts. Actual pe/vvt/vn/cCe/dCe/Mn and header fragment execute here.
context.initialModel='launch';
const models=['claude-opus-4-7','claude-opus-4-7[1m]','claude-opus-4-7[2m]','CLAUDE-OPUS-4-7','claude-sonnet-4-6','other'];
const identities=Object.fromEntries(models.map(model=>[model,{wire:context.Mn(model),family:context.Be(context.Mn(model))}]));
const slotValues=[undefined,null,'origin'];
const operations=[];
for(const override of [undefined,null,'fallback','FALLBACK'])for(const previousOverride of slotValues)for(const previousAppStateModel of slotValues)for(const previousModelForSession of slotValues)for(const carriedEffort of [undefined,null,'high'])for(const nextEffort of [undefined,null,'low']){
 const initial={fallbackModel:'fallback',previousOverride,previousAppStateModel,previousModelForSession,carriedEffort};
 const next={fallbackModel:'next',previousOverride:'new-origin',previousAppStateModel:'new-origin',previousModelForSession:'new-origin',carriedEffort:nextEffort};
 context.host=context.fresh();context.host.modelSelection.overrideMainLoopModel(override);context.host.modelSelection.replaceRefusalFallbackModelLatch(initial);context.vvt(next);
 operations.push({operation:'latch',initial,override,next,expected:{latch:context.host.modelSelection.refusalFallbackModelLatch(),end:context.host.modelSelection.takeCarriedEffortEnd()??null}});
 context.host=context.fresh();context.host.modelSelection.overrideMainLoopModel(override);context.host.modelSelection.replaceRefusalFallbackModelLatch(initial);const restoration=context.vn();context.host.modelSelection.forgetRefusalFallbackOccurred();
 operations.push({operation:'transition',initial,override,expected:{latch:context.host.modelSelection.refusalFallbackModelLatch()??null,override:context.host.modelSelection.mainLoopModelOverride(),restoration:restoration??null,end:context.host.modelSelection.takeCarriedEffortEnd()??null}});
}
const resumeCases=[];
for(const requestedModel of models)for(const fallbackModel of models)for(const neutralized of [undefined,false,true])for(const fork of [false,true])for(const category of [undefined,'cyber','bio'])for(const sawCyber of [undefined,false,true]){
 const frame={type:'system',subtype:'model_refusal_fallback',fallbackModel,neutralizedByFork:neutralized,apiRefusalCategory:category,sawCyberRefusal:sawCyber,requestId:'origin-id'};
 context.host=context.fresh();context.host.modelSelection.overrideMainLoopModel('before');context.dCe([frame],fork);
 const accepted=context.cCe([frame],requestedModel,fork,undefined,undefined);
 resumeCases.push({requestedModel,frames:[frame],fork,expected:{accepted:accepted!==undefined,override:context.host.modelSelection.mainLoopModelOverride(),latch:context.host.modelSelection.refusalFallbackModelLatch()??null,armed:context.host.modelSelection.refusalFallbackHeaderArmed(),origin:context.host.modelSelection.refusalFallbackLatchOriginRequestId()??null}});
}
const routeHistory=[
 {type:'system',subtype:'model_refusal_fallback',scope:'session',originalModel:'A',fallbackModel:'B',requestId:'req-A-B'},
 {type:'system',subtype:'model_refusal_fallback',scope:'session',originalModel:'B',fallbackModel:'C',requestId:'req-B-C'},
];
function selectionSnapshot(){
 const selection=context.host.modelSelection;
 return {override:selection.mainLoopModelOverride(),latch:selection.refusalFallbackModelLatch()??null};
}
context.host=context.fresh();
context.host.modelSelection.overrideMainLoopModel('A');
context.vvt({fallbackModel:'B',previousOverride:undefined,previousAppStateModel:'A',previousModelForSession:'A',carriedEffort:undefined});
context.host.modelSelection.overrideMainLoopModel('B');
context.vvt({fallbackModel:'C',previousOverride:'B',previousAppStateModel:'B',previousModelForSession:'B',carriedEffort:undefined});
context.host.modelSelection.overrideMainLoopModel('C');
const hotBeforeClear=selectionSnapshot();
const hotClear=context.vn();
const hotAfterClear=selectionSnapshot();

context.host=context.fresh();
context.host.modelSelection.overrideMainLoopModel('launch-model');
context.initialModel='launch-model';
const coldRestored=context.cCe(routeHistory,'C',false,undefined,undefined);
const coldBeforeClear=selectionSnapshot();
const coldClear=context.vn();
const coldAfterClear=selectionSnapshot();

// Cold resume chooses a model from the latest assistant row. cCe then uses
// the latest refusal notice only to decide whether to rebuild a latch.
const localScopeHistory=[
 {type:'system',subtype:'model_refusal_fallback',scope:'session',originalModel:'A',fallbackModel:'B'},
 {type:'system',subtype:'model_refusal_fallback',scope:'local',originalModel:'B',fallbackModel:'C'},
 {type:'assistant',message:{model:'C'}},
];
const loaderContext=vm.createContext({
 hJo:['A','B','C','launch-model'],Ra:'__default',Be:value=>value,Rt:value=>value,Kd:value=>typeof value==='string',
 oy:()=>undefined,oM:()=>false,zbe:()=>false,sy:()=>true,Ur:()=>false,HZ:()=>false,can:()=>undefined,X2:()=>false,
 Eze:()=>false,qle:async()=>undefined,i:()=>{},d:value=>value,me:value=>value,
});
vm.runInContext(`${wire}${assistantModelChooser}${resumeModelLoader}`,loaderContext);
const localScopeSelectedModel=await loaderContext.lCe(localScopeHistory,'launch-model');
context.host=context.fresh();
context.initialModel='launch-model';
const localScopeColdLatch=context.cCe(localScopeHistory,localScopeSelectedModel,false,undefined,undefined);
const localScopeColdState=selectionSnapshot();
const assistantWinsHistory=[...routeHistory,{type:'assistant',message:{model:'B'}}];
const assistantWinsModel=await loaderContext.lCe(assistantWinsHistory,'launch-model');
context.host=context.fresh();
context.initialModel='launch-model';
const assistantWinsRestore=context.cCe(assistantWinsHistory,assistantWinsModel,false,undefined,undefined);
const assistantWinsState=selectionSnapshot();
const changedOriginals=routeHistory.map((notice,index)=>({...notice,originalModel:`unrelated-${index}`}));
context.host=context.fresh();
context.initialModel='launch-model';
const changedOriginalsRestore=context.cCe(changedOriginals,'C',false,undefined,undefined);
const changedOriginalsState=selectionSnapshot();

const multiNoticeResumeCases=[
 {name:'hot-A-to-B-to-C-clear-restores-A',notices:routeHistory,expected:{beforeClear:hotBeforeClear,clear:hotClear,afterClear:hotAfterClear}},
 {name:'cold-A-to-B-to-C-clear-uses-launch-snapshot-not-notice-originalModel',notices:routeHistory,selectedModel:'C',restoredModel:coldRestored,expected:{beforeClear:coldBeforeClear,clear:coldClear,afterClear:coldAfterClear}},
 {name:'cold-loader-reads-assistant-row-and-cCe-does-not-filter-local-scope',notices:localScopeHistory.slice(0,2),assistantModel:'C',scopeOfLatestNotice:'local',selectedModel:localScopeSelectedModel,restoredModel:localScopeColdLatch,expected:{state:localScopeColdState}},
 {name:'cold-loader-assistant-model-wins-when-it-disagrees-with-latest-notice',notices:routeHistory,assistantModel:'B',selectedModel:assistantWinsModel,restoredModel:assistantWinsRestore,expected:{state:assistantWinsState}},
 {name:'cold-cCe-ignores-each-notices-originalModel-field',notices:changedOriginals,selectedModel:'C',restoredModel:changedOriginalsRestore,expected:{state:changedOriginalsState}},
];
const ids=[null,'','a','a ',' a','a\t','a\n','é','😀','a'.repeat(255),'a'.repeat(256),'!','~','x y','x\u007f','x\u00a0','x  '];
const headerCases=[];
for(const firstParty of [false,true])for(const armed of [false,true])for(const occurred of [false,true])for(const lane of [false,true])for(const id of ids){headerCases.push({firstParty,armed,occurred,lane,id,validated:context.efn(id)??null,expected:context.headers(firstParty,armed,occurred,lane,id)});}
const source={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,scope:'Actual model-selection storage, latch/transition and resume/header predicates. Bare-wire family branch has supplied absent served aliases, overrides and transport parsing. Initial launch model, provider/feature gates, transcript frames and origin ids are explicit facts; full host acquisition, server-swapped response/effort handling and complete JSONL envelopes remain separate.',identities,operations,resumeCases,multiNoticeResumeCases,resumeFacts:{coldModelLoader:'lCe calls N(history, configuredModel); N scans backward for a non-meta assistant message.message.model. It does not select from a refusal notice.',latestNoticeSelector:'ie selects the latest system row with subtype model_refusal_fallback and does not inspect scope.',resumeLatchCondition:'cCe compares that notice.fallbackModel to the model already selected by lCe; originalModel does not participate.',coldLatchSlots:'previousOverride is undefined; previousAppStateModel is KL() ?? null; previousModelForSession is explicitly null.',hotClear:'vvt keeps the first live latch restoration slots across a B-to-C hop; vn clears only while the current override is still the latched fallback and returns those saved slots.',providerProfiles:'The native model-selection latch has no provider-profile slot; profile restoration in Harness is a host extension.'}};
const sdk={version:source.version,native_sha256:source.native_sha256,sourceByteRanges:ranges,headerCases};
for(const [file,data] of [[new URL('../../crates/core/tests/fixtures/refusal_state_2_1_288.json',import.meta.url),source],[new URL('../../deps/llm-client/tests/fixtures/refusal_headers_2_1_288.json',import.meta.url),sdk]]){
 const text=JSON.stringify(data,null,2)+'\n';if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
}
console.log(JSON.stringify({operations:operations.length,resumeCases:resumeCases.length,headerCases:headerCases.length,ranges:ranges.length}));
