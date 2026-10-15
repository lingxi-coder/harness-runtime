#!/usr/bin/env node
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
import {nativeBetaWorld} from './common/anthropic_native_288.mjs';
const {binary,hash,ranges,take,context,error}=nativeBetaWorld(process.argv[2]);
const canonical=take('function qI(e)', 'function ',181784550,'native bare-wire identity branch');
const date=take('function Bb(e)', 'function ',181786200,'native date identity suffix');
const contextTag=take('function kt(e){return e.replace', 'function ',179326600,'native context identity suffix');
const speed=take('function oFe(e,n)', 'function rFe(',188948300,'actual speed/model recognition');
const history=take('function Q4o(e)', 'function Owe(',181686350,'actual rejection, rearm and history lookup');
const hostClass=take('class u_{', 'var Zl=',181683500,'actual host fast-state reset');
const ir=take('x4e=(qn)=>', ',A4e=',189000000,'complete actual IR composition');
vm.runInContext(`var Mee=['us','eu','apac','global'];function sie(){return undefined;}function Iu(e){return {base:e};}function TW(){return false;}
${canonical}${date}${contextTag}
function Nm(e){return kt(qI(e.toLowerCase()));}function Ie(){return {emit(){},subscribe(){}};}
${hostClass}var Zl=new u_();function freshHost(){return new u_();}${history}${speed}
var ${ir};`,context);
const models=['claude-opus-4-7','claude-opus-4-7[1m]','claude-opus-4-7-20260901','claude-sonnet-4-6','other-20260101','other-20260101[1m]','other'];
const identities=Object.fromEntries(models.map(model=>[model,context.Nm(model)]));
const messages=[...models.map(model=>`'${model}' does not support the \`speed\` parameter`),
 "'other' does not support the `speed` parameter; 'claude-opus-4-7' does not support the `speed` parameter",
 "'' does not support the `speed` parameter", "'claude-opus-4-7' does not support the speed parameter",
 'Fast mode is not enabled','fast mode is not enabled','prompt is too long','fixture unknown request rejection',"'claude-opus-4-7' does not support the `speed` parameterX",
];
const cases=[];
for(const request_model of models)for(const status of [400,422,500])for(const message of messages)for(const target of [false,true])for(const rejected of [false,true])for(const declined of [false,true]){
 const body={type:'error',error:{type:'invalid_request_error',message}},e=error(status,body,declined);
 context.Zl=context.freshHost();if(rejected)context.Zl.everFastRejectedModels.add(context.Nm(request_model));
 context.V={model:request_model,modelIsRefusalFallbackTarget:target};
 cases.push({request_model,status,body,target,rejected,declined,speedMatches:context.oFe(e,request_model),fastNotEnabled:context.Yle(e),expected:context.IR(e)});
}
const states=[];
for(const operation of ['reject','rearm','reset'])for(const blocked of [false,true])for(const ever of [false,true]){
 context.Zl=context.freshHost();if(blocked)context.Zl.fastRejectedFallbackModels.add(context.Nm(models[0]));if(ever)context.Zl.everFastRejectedModels.add(context.Nm(models[0]));
 if(operation==='reject')context.Q4o(models[0]);else if(operation==='rearm')context.mQe();else context.Zl.reset();
 states.push({operation,blocked,ever,expected:{blocked:context.Y6t(models[0]),ever:context.Zl.everFastRejectedModels.has(context.Nm(models[0])),recognized:context.ttr(models[0])}});
}
const output={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,
 scope:'Actual IR/GGt/oFe and Q4o/mQe/ttr/host reset. Identity uses actual qI/Bb/kt bare-wire branch with served aliases, transport parser and model overrides supplied absent. Host target acquisition, full notices and other model/flag/routing acquisition remain separate requirements.',identities,cases,states};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../deps/llm-client/tests/fixtures/fast_refusal_2_1_288.json',import.meta.url);
const coreFile=new URL('../../crates/core/tests/fixtures/fast_refusal_2_1_288.json',import.meta.url),coreText=JSON.stringify({version:output.version,native_sha256:output.native_sha256,sourceByteRanges:ranges,states},null,2)+'\n';
if(process.argv.includes('--check')){assert.equal(readFileSync(file,'utf8'),text);assert.equal(readFileSync(coreFile,'utf8'),coreText);}else{writeFileSync(file,text);writeFileSync(coreFile,coreText);}
console.log(JSON.stringify({cases:cases.length,states:states.length}));
