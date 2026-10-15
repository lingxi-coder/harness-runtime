#!/usr/bin/env node
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync} from 'node:fs';
import {nativeBetaWorld} from './common/anthropic_native_288.mjs';
const {binary,hash,ranges,context,betaHeaders,messages,error}=nativeBetaWorld(process.argv[2]);
const cases=[];
for(const status of [200,400,422,500])for(let mask=0;mask<16;mask++)for(const message of messages)for(const body of [
 {type:'error',error:{type:'invalid_request_error',message}}, {message}, message,
]){
 const sent=betaHeaders.filter((_,index)=>mask&(1<<index)),e=error(status,body);
 context.kw=sent;context.Qe=[context.Vfe,context.W2e,context.cEe,context.aY];context.qe={rejected:new Set()};context.modelRejected=new Map();
 const named=context.kMe(e,sent).map(beta=>({beta:beta.name,strong:context.SMe(e,beta)}));
 const token=context.E3e(e),directive=token??context.T3e(e)??null;
 cases.push({status,body,sent,named,expected:{directive,rejected:[...context.qe.rejected],modelRejected:[...(context.modelRejected.get('fixture')??[])]}});
}
const recognizedMessages=[...messages,
 'prompt is too long','input is too long for requested model','input length and `max_tokens` exceed context limit',
 'input length and max_tokens exceed context limit','capability_rejected: prompt_too_long','capability_rejected: prompt_too_long_extra',
 'credit balance is too low','organization has been disabled','Fast mode is not enabled','fast mode is not enabled',
 'cannot be used as an advisor when the request model is foo','Output blocked by content filtering policy',
 'role system must precede an assistant','use the top-level `system` parameter for the initial system prompt',
 'text-only role `system` messages require beta','Unexpected role input message role','cache_control on system messages',
 'role "system" not supported','role xxxsystem not supported','capability_rejected: mid_conv_system',
 'invalid `encrypted_content` in search_result block','invalid encrypted_stdout in encrypted_code_execution_result block',
 'bound to a different conversation','signature in thinking block',
];
const recognitionCases=[];
for(const status of [0,200,400,401,422,500])for(const declined of [false,true])for(const message of recognizedMessages){
 const body={type:'error',error:{type:'invalid_request_error',message}},e=error(status,body,declined);
 recognitionCases.push({status,body,declined,messageBytes:e.message,expected:context.GGt(e)});
}
for(const status of [200,400,422,500])for(const body of [null,{},false,0,[],{message:'',error:{type:'authentication_error',message:'unknown'}},{message:'top-level unknown',error:{message:'prompt is too long'}},{error:{type:'policy_blocked',message:'unknown'}},{error:{type:'permission_error',message:'unknown'}},{error:{type:'invalid_request_error',details:{error_code:'dlp_request_denied'},message:'unknown'}}]){
 const e=error(status,body);recognitionCases.push({status,body,declined:false,messageBytes:e.message,expected:context.GGt(e)});
}
const output={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,scope:'Actual APIError, complete GGt composition and kMe/SMe/Gt, with first-party E3e before T3e. Model key, lifecycle, host sinks and the remaining pF/repair graph remain separate dependencies.',cases,recognitionCases};
const file=new URL('../../deps/llm-client/tests/fixtures/beta_repair_2_1_288.json',import.meta.url),text=JSON.stringify(output,null,2)+'\n';
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(JSON.stringify({cases:cases.length,recognitionCases:recognitionCases.length}));
