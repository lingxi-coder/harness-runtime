#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]);const sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges=[];
function take(first,last,offset,label){const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);assert(start>=offset&&end>start&&end-start<5000,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString();}
const advance=take('function ll({chain:', 'function dl(',195093300,'actual chain advancement');
const notice=take('function cp(e,o,n,r)', 'function Kl(',195179450,'actual reason-labelled fallback notice');
const suffix=take('function bl(e,o)', 'function Ms(',195096400,'actual context shrink notice producer');
const counts=take('var re={notation:', 'function sar(',180035000,'actual Intl compact format options and token formatter');
const upper=take('function Ho(e){return gr(e).toUpperCase()}', 'var Yc=',195097980,'actual upper-case context formatter');
const configured=take('function _vn(e,n)', 'function Cse(',181783710,'actual exclusion of the selected primary from an explicit chain');
const inPlace=take('Dt){t(`chain advance collapsed', 'if(ot)bLt(',195155300,'actual in-place notice suppression');
const ctx=vm.createContext({Intl,GZo:(locale,options)=>new Intl.NumberFormat(locale,options),kl:()=>ctx.window,Tl:()=>undefined,c:e=>{throw e;}});
ctx.Sb=()=>false;
vm.runInContext(configured+advance+notice+suffix+counts+upper+'function emitsNotice(Dt){for(let round=0;round<1;round++){'+'if('+inPlace+'return true}return false}',ctx);
ctx.ge='same';ctx.t=()=>{};
const configuredCases=[];
for(const primary of ["a","b"])for(const fallback of ["", "a", "b", "other/model"]){configuredCases.push({primary,fallback,expected:JSON.parse(JSON.stringify(ctx._vn(primary,fallback)))});}
const inPlaceCases=[false,true].map(in_place=>({in_place,emits_notice:ctx.emitsNotice(in_place)}));
assert.deepEqual(inPlaceCases,[{in_place:false,emits_notice:true},{in_place:true,emits_notice:false}]);
const chains=[[],['a'],['b'],['a','a'],['a','b'],['b','a'],['a','b','a'],['a','b','c'],['A','a'],['provider/model','other/model'],['same','same','same']];
const chainCases=[];
for(const chain of chains)for(const from of [0,1,2,3,4])for(const failed of ['a','b','c','same','provider/model'])for(const reason of ['overloaded','server_error','model_not_found','permission_denied','model_blocked','last_resort']){
 const out=ctx.ll({chain,from,failedModel:failed,reason,substituteFor:()=>undefined});
 chainCases.push({chain,from,failed,reason,expected:{index:out.chainIndex,target:out.target}});
}
const windows=[0,1,999,1000,1049,1050,999949,999950,1000000,1050000,128000,200000,1000000000,1000000000000];
const noticeCases=[];
for(const from of ['Opus 4.7','provider/model','模型🙂'])for(const to of ['Sonnet 4.6','Model B'])for(const fromWindow of windows)for(const toWindow of windows){
 ctx.window={fromWindow,toWindow};const text=ctx.cp('server_error',from,to)+ctx.bl({},{}).noticeSuffix;
 noticeCases.push({from,to,from_window:fromWindow,to_window:toWindow,expected:text});
}
const data={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual current native chain advancement without availability substitution; server-error notice and context shrink producer with supplied window facts and no overloaded keep-window hint. Availability/model discovery and CLI lifecycle are separate.',chainCases,noticeCases,inPlaceCases,configuredCases};
const text=JSON.stringify(data,null,2)+'\n',path=new URL('../../crates/orchestrator/tests/fixtures/model_fallback_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text);else writeFileSync(path,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${chainCases.length} native chain and ${noticeCases.length} notice cases`);
