#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),digest=b=>createHash('sha256').update(b).digest('hex');
assert.equal(digest(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){
 const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=base&&end>start,label);
 const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:digest(bytes)});return bytes.toString('utf8');
}
const disable=take(179726616,'function rK(){','function Y_t(e)','rK environment/HIPAA');
const truth=take(177878934,'function Le(e){','function IT(e)','Le/gs bool parser');
const parser=take(178161833,'function i(n){','export{B7o,D}','D typed environment parsers');
const boolSlot=take(178163242,'jE=D.bool()',',JE=D.bool()','experimental-beta bool schema');
const getter=take(178163242,'function ZZe(t,o){','var a=ZZe','ZZe environment getter');
const cloud=take(179125203,'function GF(e=He()){','function ja(){','GF/zF actual provider eligibility');
const own=take(180615120,'function own(e){','function rN()','own model eligibility');
const features=take(185762130,'function YMe(e,n,r,s,g){','async function nPo(e','YMe/YLo/ZLo config feature insertion');
const peel=take(185762130,'let wa={...ji.output_config','let Ya=z.refusalFallback','main extra config peel and automatic fields');
const mainTemplate=take(185762130,'let pd={model:Tx(z.model)',';hge=Q6','main final object spread');
const sideGate=take(185762130,'let $t=Boolean(b)&&','let It=dLr(','side format admission');
const sideConfig=take(185762130,'let rn={...$t&&','Qt=QD(e.querySource)','side config assembly');
const registration=take(179726616,'Koe=R("structured_outputs",',',ner=R(','beta registration');
const structuredBeta=JSON.parse(registration.slice('Koe=R("structured_outputs",'.length,-1));
const extraCases=[{}, {foo:true}, {output_config:{}}, {output_config:null}, {output_config:false}, {output_config:17},
 {output_config:{effort:'low',format:{type:'explicit'}}}, {output_config:{format:null}},
 {output_config:{effort:null}}, {output_config:{effort:0}}, {output_config:{effort:false}}, {output_config:{effort:[]}}, {output_config:{effort:{unknown:true}}}, {output_config:[]}, {output_config:['x',null]}, {output_config:'ab'},
 {output_config:'💥'}, {output_config:{'2':'two','0':'zero','01':'leading','4294967295':'limit',unknown:true}},
 {output_config:{format:'explicit'},before:1,after:2,speed:'slow'}, {max_tokens:7,metadata:{user_id:'override'},speed:'slow'},
 {output_config:{task_budget:null},'3':'three','0':'zero',extension:{'2':2,'0':0}}];
const modelOrder=take(180615120,'var av=[','function nwn()','actual av/lr model-order comparison');
const cases=[];
for(const kind of ['main','side'])for(const supported of [false,true])for(const effort of [null,'high',0,7,false,{},[]])for(const format of [false,true])for(const speed of [null,'fast'])for(const extra of extraCases){
 if(kind==='side'&&speed!==null)continue;
 const input={kind,supported,effort,format,speed,extra};
 const expected=vm.runInNewContext(`(()=>{
  const process={env:{}},Oc=Number;
  ${truth}${parser}${getter}let ${boolSlot};const a=ZZe({CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:jE},null);
  const ol=()=>false,He=()=> 'firstParty',Be=x=>x,cc=()=>He(),a_e=()=>true,jw=()=>INPUT.supported;${modelOrder}
  const Koe=STRUCTURED_BETA,I2e={header:'effort-2025-11-24'},rer={header:'task-budgets-2026-03-13'},Im=()=>true;
  ${disable}${cloud}${own}${features}
  const schema={type:'json_schema',schema:{type:'object',properties:{ok:{type:'boolean'}},required:['ok'],additionalProperties:false}};
  let base,body,betas=[];
  if(INPUT.kind==='main'){
   const z={model:'claude-sonnet-4-6',outputFormat:INPUT.format?schema:undefined};
   const ji=JSON.parse(JSON.stringify(INPUT.extra)),sr=betas,We=z.model,mr=INPUT.effort??undefined;
   const Gn={model:We},vs='firstParty',NRe=()=>{};
   ${peel}
   const Tx=x=>x,fh=null,Ou=[{role:'user',content:'fixture'}],Ka={system:[]},Lh=undefined,$s=false,ru=[],sT=x=>x.map(v=>v.header),ZFe=()=>({user_id:'fixture'}),Ih=16;
   const Zc=undefined,Hj=undefined,Qv=undefined,Rb=false,twe={},vge=false,Al=undefined,Nh=false,zC=null,Jv=null,Fs={},np=INPUT.speed??undefined,Uy=()=>false,Qe={},Upe={},qe=false,Ne=undefined;
   ${mainTemplate};body=pd;
   base={model:We,messages:Ou,system:[],metadata:ZFe(),max_tokens:16};
  }else{
   const b=INPUT.format?schema:undefined,ht=fn=>fn(),We='claude-sonnet-4-6',kt=betas,un={};
   ${sideGate}
   if(INPUT.effort!==null)YMe(INPUT.effort,un,{},kt,We);
   ${sideConfig}dummy=undefined;
   base={model:We,max_tokens:16,system:[],messages:[{role:'user',content:'fixture'}]};
   body={...base,...Yt&&{output_config:rn},metadata:{user_id:'fixture'},...INPUT.extra};
   base={...base,metadata:{user_id:'fixture'}};
  }
  // Feed the same codec-computed fields into the SDK policy. The oracle's final
  // bytes above remain the untouched native object-template result.
  if(INPUT.effort!==null)base.output_config={effort:INPUT.effort};
  if(INPUT.format)base.output_config={...base.output_config,format:schema};
  if(INPUT.kind==='main'&&INPUT.speed!==null)base.speed=INPUT.speed;
  return {base,body_json:JSON.stringify(body),betas:betas.map(v=>typeof v==='string'?v:v.header)};
 })()`,{INPUT:input,STRUCTURED_BETA:structuredBeta});
 cases.push({input,expected:JSON.parse(JSON.stringify(expected))});
}
const eligibility=[];
for(const provider of ['firstParty','bedrock','vertex','foundry','anthropicAws','anthropicGoogleCloud','mantle','gateway'])for(const env of [null,'','1','0','false','true',' ','FALSE','yes','on','OFF','wat',' true ','\uFEFFtrue\uFEFF','\u0085true\u0085'])for(const model of ['claude-sonnet-4-6','claude-opus-4-1','claude-opus-4-0','claude-sonnet-4-0','claude-3-5-sonnet']){
 const input={provider,env,model};
 const expected=vm.runInNewContext(`(()=>{const process={env:INPUT.env===null?{}:{CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:INPUT.env}},Oc=Number;${truth}${parser}${getter}let ${boolSlot};const a=ZZe({CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:jE},null),ol=()=>false,He=()=>INPUT.provider,cc=()=>He(),Be=x=>x;${modelOrder}${disable}${cloud}${own}return own(INPUT.model);})()`,{INPUT:input});
 eligibility.push({input,expected});
}
const output={version:'2.1.287',native_sha256:digest(binary),sourceByteRanges,scope:'Exact main YMe/YLo/ZLo peel and final pd template, side gate/config plus final extra spread, GF/zF/own route/model gates. Effort support is an explicit input covering both outcomes; dynamic Foundry capability permission is supplied true, HIPAA false. Captures policy bytes, not the entire native SDK/CLI request.',cases,eligibility};
assert.equal(cases.length,1764);assert.equal(eligibility.length,600);
const json=JSON.stringify(output,null,2)+'\n';
const destinations=[new URL('../../deps/llm-client/tests/fixtures/output_config_wire_2_1_287.json',import.meta.url),new URL('../../crates/llm-runtime/tests/fixtures/output_config_wire_2_1_287.json',import.meta.url)];
for(const destination of destinations){const file=fileURLToPath(destination);if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),json);else writeFileSync(file,json);}
process.stdout.write(`${process.argv.includes('--check')?'checked':'wrote'} native output-config wire: ${cases.length} byte cases, ${eligibility.length} route/model cases\n`);
