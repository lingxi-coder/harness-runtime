// Regenerate using the extracted official @anthropic-ai/claude-code-darwin-arm64 2.1.270.
// Usage: node generate.mjs /tmp/lingxi-loop-oracle-2.1.270
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import crypto from 'node:crypto';
import {fileURLToPath} from 'node:url';
const dir = path.dirname(fileURLToPath(import.meta.url));
const root = process.argv[2];
const sha = b => crypto.createHash('sha256').update(b).digest('hex');
const binary = fs.readFileSync(path.join(root, 'package/claude'));
const binaryHash = 'a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807';
if (sha(binary) !== binaryHash) throw Error('Unexpected oracle binary');
const sources = ['src_192949468.js','src_186131770.js'].map(n => fs.readFileSync(path.join(root,'chunks',n),'utf8'));
const sourceHashes = ['8d85e6c776e792462d2645387323fb45efc508070fe9ffbb8121ca37ca4845fb','76d796406e2e9124ed51549eaf863d4c8a9f8f33c6c596c4ef315dbb0e1a0a98'];
for (const [i, source] of sources.entries()) {
 if (sha(source) !== sourceHashes[i]) throw Error('Unexpected extracted chunk');
 if (!binary.includes(Buffer.from(source))) throw Error('Chunk not present verbatim in oracle binary');
}
const preambles = ['loopAutonomousPreamble.md','loopAutonomousPreamblePersistent.md'].map(n => {
 const b = fs.readFileSync(path.resolve(dir,'../../../../cron/src/bundled',n));
 if (!binary.includes(b)) throw Error(`Preamble not present verbatim in binary: ${n}`);
 return b.toString();
});
const common = {ga:'ScheduleWakeup',da:'Monitor',uS:'TaskList',Kg:'TaskStop',UR:'PushNotification',h0e:'<<autonomous-loop>>',cle:'<<autonomous-loop-dynamic>>'};
const commands=[],ticks=[];
for (const persistent of [false,true]) for (const push of [false,true]) {
 for (const timeout of [0,600000,1800000]) {
 const ctx = vm.createContext({...common, wee:()=>push,Az:()=>timeout!==0,Vye:()=>timeout,Xxt:n=>`${Math.round(n/60000)} minutes`,Ym:'CronCreate',cw:'CronDelete',See:7,a:{CLAUDE_CODE_REMOTE:true},bt:()=>false,s:{LOOP_FILE_SENTINEL:'<<loop.md>>',LOOP_FILE_DYNAMIC_SENTINEL:'<<loop.md-dynamic>>',logAutonomousLoopActivation(){},getAutonomousLoopPreamble:()=>preambles[+persistent]}});
 const src=sources[0].slice(sources[0].indexOf('N="10m"')).replace(/^N=/,'var N=');
 vm.runInContext(src.slice(0,src.indexOf('function Z()')),ctx);
 for (const [name,expression] of [['prompt','S("check the deploy")'],['auto_dynamic','f(null,true,"10m")'],['auto_cron','f(null,false,"10m")'],['file_dynamic','f({path:"/tmp/proj/loop.md",content:"- task A\\n- task B"},true,"5m")'],['file_cron','f({path:"/tmp/proj/loop.md",content:"- task A\\n- task B"},false,"5m")']]) commands.push({name,persistent,push,timeout,text:vm.runInContext(expression,ctx)});
 }
 const ctx=vm.createContext({...common,a:{CLAUDE_CODE_LOOP_PERSISTENT:persistent},H:()=>false,wee:()=>push,i(){},p:preambles[0],y:preambles[1]});
 const src=sources[1].slice(sources[1].indexOf('function g()'));
 vm.runInContext(src.slice(0,src.indexOf('export{')),ctx);
 for (const [name,fn] of [['auto_cron','b'],['auto_dynamic','E'],['file_cron','C'],['file_dynamic','F'],['absent_dynamic','M']]) ticks.push({name,persistent,push,text:vm.runInContext(`${fn}()`,ctx)});
}
fs.writeFileSync(path.join(dir,'commands.json'),JSON.stringify(commands,null,2)+'\n');
fs.writeFileSync(path.resolve(dir,'../../../../cron/src/bundled/loop_ticks_2_1_270.json'),JSON.stringify(ticks,null,2)+'\n');
fs.writeFileSync(path.join(dir,'provenance.json'),JSON.stringify({version:'2.1.270',binarySha256:binaryHash,chunks:sources.map((s,i)=>({name:['src_192949468.js','src_186131770.js'][i],sha256:sha(s)})),preambles:preambles.map(s=>({bytes:Buffer.byteLength(s),sha256:sha(s)})),normalizations:[],commandVariants:commands.length,tickVariants:ticks.length,cloudOffer:'disabled (CLAUDE_CODE_REMOTE=true)',monitorTimeouts:[0,600000,1800000]},null,2)+'\n');
console.log(`Generated ${commands.length} command and ${ticks.length} tick variants`);
