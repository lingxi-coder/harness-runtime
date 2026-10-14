#!/usr/bin/env node
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
const binary=fs.readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),ranges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(base,a,b,label){const start=binary.indexOf(a,base),end=binary.indexOf(b,start+a.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const builders=take(211390781,'A="10m"','function se(){','loop command builders');
const visibility=take(185217601,'function Mmn(','var s=','status and outcome visibility');
const preambles=['loopAutonomousPreamble.md','loopAutonomousPreamblePersistent.md'].map(name=>{let bytes=fs.readFileSync(new URL(`../../crates/cron/src/bundled/${name}`,import.meta.url));assert(binary.includes(bytes),name);return bytes.toString('utf8');});
const cases=[];
for(const persistent of [false,true])for(const push of [false,true])for(const timeout of [0,600000,1800000])for(const prearm of [false,true])for(const brief of [false,true]){
 const context=vm.createContext({qa:'ScheduleWakeup',Ua:'Monitor',jA:'TaskList',Tc:'TaskStop',mk:'PushNotification',qp:'Reply',Kg:'CronCreate',xv:'CronDelete',rSe:7,aSe:'<<autonomous-loop-dynamic>>',K1e:'<<autonomous-loop>>',Gs:'AskUserQuestion',So:'Skill',uX:'cloud-loop',a:{CLAUDE_CODE_REMOTE:true},HB:()=>brief,sSe:()=>push,MJ:()=>timeout!==0,q1e:()=>timeout,Omn:n=>`${Math.round(n/60000)} minutes`,Zze:()=>null,Pt:()=>false,Vn:()=>false,ft:()=>false,on:()=>false,lh:()=>[],h:{LOOP_FILE_SENTINEL:'<<loop.md>>',LOOP_FILE_DYNAMIC_SENTINEL:'<<loop.md-dynamic>>',logAutonomousLoopActivation(){},getAutonomousLoopPreamble:()=>preambles[+persistent]}});
 vm.runInContext('var '+builders+';'+visibility,context);
 for(const [name,expression] of [['prompt',`R('check the deploy',${prearm})`],['auto_dynamic',`y(null,true,'10m',${prearm})`],['auto_cron',`y(null,false,'10m',${prearm})`],['file_dynamic',`y({path:'/tmp/proj/loop.md',content:'- task A\\n- task B'},true,'5m',${prearm})`],['file_cron',`y({path:'/tmp/proj/loop.md',content:'- task A\\n- task B'},false,'5m',${prearm})`]]){
  cases.push({name,persistent,push,timeout,prearm,brief,text:vm.runInContext(expression,context)});
 }
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:ranges,scope:'Actual loop command templates, status/outcome ordering, pre-arm and brief visibility, bounded monitors and current preambles. Pre-arm model-gate result supplied explicitly; selected-model mitigation acquisition, dispatch and tick lifecycle excluded. PR stewardship and cloud offers supplied absent.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/command-api/tests/fixtures/loop_command_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(fs.readFileSync(destination,'utf8'),json);else fs.writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native loop command cases`);
