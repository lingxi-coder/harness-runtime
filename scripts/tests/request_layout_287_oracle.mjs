#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const ranges=[];
function take(startMarker,endMarker,label){const start=binary.indexOf(startMarker,185762130),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=185762130&&end>start);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const main=take('let pd={model:Tx(z.model)',';hge=Q6','main pd object template');
const side=take('En={model:Tx(We)',';if(YTn(En))','side En object template');
const extras=[{}, {extension:true}, {model:'override',max_tokens:7,metadata:{user_id:'override'},stream:false,speed:'slow',thread:'extra',diagnostics:'extra'}, {'2':2,'0':0,before:'one',after:'two'}];
const cases=[];
for(const kind of ['main','side'])for(const full of [false,true])for(const streaming of [false,true])for(const extra of extras)for(const reverse of [false,true]){
 if(kind==='side'&&streaming)continue;
 let base={max_tokens:16,model:'claude-sonnet-4-6',metadata:{user_id:'fixture'},messages:[{role:'user',content:'fixture'}],system:[]};
 if(full){Object.assign(base,{tools:[],tool_choice:{type:'any'},temperature:0.5,thinking:{type:'adaptive'},output_config:{effort:'high',format:{type:'json_schema',schema:{type:'object'}}}});if(kind==='main')Object.assign(base,{context_management:{fixture:true},safeguards:[{type:'fixture',classifier_context:{fixture:true}}],speed:'fast',thread:{id:'thread'},diagnostics:{previous_message_id:'previous'}});else base.stop_sequences=['stop'];}
 if(streaming)base.stream=true;
 if(reverse)base=Object.fromEntries(Object.entries(base).reverse());
 const input={kind,base,extra};
 const expected=vm.runInNewContext(`(()=>{
 const B=INPUT.base,Tx=x=>x;let body;
 if(INPUT.kind==='main'){
 const z={model:B.model},fh=B.thread?{sliceFrom:0,thread:B.thread}:null,Ou=B.messages,Gl=[],Ka={system:B.system,tools:B.tools},Lh=B.tool_choice,$s=false,ru=[],sT=x=>x,ZFe=()=>B.metadata,Ih=B.max_tokens,Zc=B.thinking,Hj=B.temperature,Qv=B.context_management,Rb=true,sr=['ctx'],twe='ctx',vge=Boolean(B.safeguards),Al=B.safeguards?.[0].classifier_context,LPe='fixture',Nh=false,zC=null,Jv=null,Fs={},ji=INPUT.extra,wa=B.output_config??{},np=B.speed,Uy=()=>Boolean(B.diagnostics),Qe={},Upe={},qe=Boolean(B.diagnostics),Ne=B.diagnostics?.previous_message_id;
 ${main};body=pd;if(B.stream!==undefined)body={...body,stream:B.stream};
 }else{
 const We=B.model,Xt=false,w=B.max_tokens,nn=B.system,Pn=B.messages,g=B.tools,h=B.tool_choice,Yt=Boolean(B.output_config),rn=B.output_config?{format:B.output_config.format,effort:B.output_config.effort}:undefined,_e=B.temperature,ht=fn=>fn(),PQn=()=>true,$n=B.model,ve=B.stop_sequences,jt=B.thinking,kt=[],sT=x=>x,ZFe=()=>B.metadata,Te=INPUT.extra,qn=()=>({});
 let ${side};body=En;
 }
 return {body_json:JSON.stringify(body)};
 })()`,{INPUT:input});cases.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:ranges,scope:'Unmodified main pd and side En templates; feature values supplied as computed codec fields. Includes final main stream spread, key collisions and computed tails. Config admission/extra config handled by the separate 204-case oracle; not entire native CLI.',cases};
const json=JSON.stringify(output,null,2)+'\n';
for(const path of ['../../deps/llm-client/tests/fixtures/request_layout_2_1_287.json','../../crates/llm-runtime/tests/fixtures/request_layout_2_1_287.json']){const destination=new URL(path,import.meta.url);if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);}
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native request layout cases`);
