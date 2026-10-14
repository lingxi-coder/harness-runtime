#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),sourceByteRanges=[];
assert.equal(sha(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
function take(base,first,last,label){const start=binary.indexOf(first,base),end=binary.indexOf(last,start+first.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const truth=take(178927000,'function Le(e){','function zT(e)','actual Le/_s bool parsing');
const parser=take(179192500,'function i(n){','export{Its,M}','M typed environment parsers');
const getter=take(179258900,'function Ntt(o,t){','var a=Ntt','Ntt cached environment getter');
const disabledSlot=take(179225700,'zE=M.bool()',',$E=M.bool()','structured-output bool slot');
const providers=take(180071300,'function p$(','function Ka(){','actual p$/f$ provider families');
const disable=take(180413600,'function BK(){','function Hbt(','actual experimental/HIPAA disable');
const order=take(181801000,'var Ev=[','function Cvn(){','actual Ev/sr chronological model comparison');
const eligible=take(181806200,'function Tvn(','function EN(){','actual Tvn eligibility');
const main=take(188962700,'function wWo(','async function hHo(','actual wWo automatic main schema');
const sideGate=take(188559800,'let Ft=Boolean(b)&&','let Bt=','actual side format/beta admission');
const sideConfig=take(188560500,'let rn={...Ft&&','fn=_D(','actual side format-before-effort assembly');
const registration=take(180410500,'XZ=w("structured_outputs",',',orr=w(','actual structured-output beta registration');
const beta=JSON.parse(registration.slice('XZ=w("structured_outputs",'.length,-1));
const envs=[null,'','1','0','false','true','yes','on','OFF','FALSE',' ','wat',' true ','\uFEFFtrue\uFEFF','\u0085true\u0085'];
const cases=[];
for(const provider of ['firstParty','anthropicAws','anthropicGoogleCloud','foundry','mantle','gateway','other'])for(const model of ['claude-sonnet-4-6','claude-opus-4-1','claude-opus-4-0','claude-3-5-sonnet','unknown'])for(const env of envs)for(const structured_env of envs)for(const capability of [false,true])for(const explicit of [false,true]){
 const input={provider,model,env,structured_env,capability,explicit,hipaa:false};
 const context=vm.createContext({INPUT:input,$c:Number,Le:undefined,He:()=>provider,pc:()=>provider,Be:x=>x,Jn:()=>false,al:()=>false,cae:()=>!capability,XZ:beta});
 vm.runInContext(truth+parser+getter+'let '+disabledSlot+';const process={env:{...INPUT.env!==null&&{CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:INPUT.env},...INPUT.structured_env!==null&&{CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS:INPUT.structured_env}}};var a=Ntt({CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:M.bool(),CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS:zE},null);'+providers+disable+order+eligible+main,context);
 const expected=vm.runInContext(`(()=>{const schema={type:'json_schema',schema:{type:'object',properties:{ok:{type:'boolean'}},required:['ok'],additionalProperties:false}};const config={effort:'low',...INPUT.explicit&&{format:{type:'explicit_fixture'}}},betas=[];wWo(schema,config,betas,INPUT.model);const b=schema,yt=fn=>fn(),Ge=INPUT.model,Ot=[],wn={effort:'low'};${sideGate}${sideConfig}dummy=undefined;return {eligible:Tvn(INPUT.model),main:{config,betas},side:{config:rn,betas:Ot,hasConfig:an}}})()`,context);
 cases.push({input,expected:JSON.parse(JSON.stringify(expected))});
}
// HIPAA taint is supplied separately from both independent parsed env flags.
for(const hipaa of [false,true])for(const env of [null,'false','true'])for(const structured_env of [null,'false','true']){
 const context=vm.createContext({INPUT:{env,structured_env},He:()=> 'firstParty',pc:()=> 'firstParty',Be:x=>x,Jn:()=>false,al:()=>hipaa,$c:Number});
 vm.runInContext(truth+parser+getter+'const process={env:INPUT};var a=Ntt({CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:M.bool(),CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS:M.bool()},null);'+providers+disable+order+eligible,context);
 // Use real env names, not the input field names, in the source getter.
 vm.runInContext('process.env={...INPUT.env!==null&&{CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:INPUT.env},...INPUT.structured_env!==null&&{CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS:INPUT.structured_env}}',context);
 cases.push({input:{provider:'firstParty',model:'claude-sonnet-4-6',env,structured_env,capability:true,explicit:false,hipaa},expected:{eligible:vm.runInContext('Tvn("claude-sonnet-4-6")',context)}});
}
const output={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual parsed bool getters, provider/model eligibility, main wWo and side automatic format/beta/config projection. Resolved canonical model, capability refusal and HIPAA taint supplied. Full model discovery, SDK transports, automatic format callers and embedded CLI acceptance excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../crates/llm-runtime/tests/fixtures/structured_output_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native structured-output cases`);
