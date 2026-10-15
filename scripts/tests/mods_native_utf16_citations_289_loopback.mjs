import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, writeFile, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

const args = process.argv.slice(2);
if (args.length === 0) {
  console.log(JSON.stringify({mode:'review-only',native_invoked:false,invocation:'node scripts/tests/mods_native_utf16_citations_289_loopback.mjs --execute <binary>',scope:'Two local queries followed by a second-process --resume; exact live Mod/API and raw JSONL, with restart and HTTP normalization reported separately.'},null,2));
  process.exit(0);
}
assert.equal(args.length,2,'expected --execute <binary>');
assert.equal(args[0],'--execute');
const binaryPath = path.resolve(args[1]);
const expectedSha = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const hash = value => createHash('sha256').update(value).digest('hex');
const binary = await readFile(binaryPath);
assert.equal(hash(binary), expectedSha);
const root = await mkdtemp(path.join(tmpdir(), 'mods-native-row-wire-289-sequential-'));
const sessionDir = path.join(root, 'session');
const configDir = path.join(root, 'config');
const pluginDir = path.join(root, 'plugin');
const tempDir = path.join(root, 'tmp');
for (const dir of [sessionDir, configDir, tempDir, path.join(pluginDir, '.claude-plugin'), path.join(pluginDir, 'hooks')]) {
  await mkdir(dir, {recursive: true});
}
const fixture = {
  '.claude-plugin/plugin.json': JSON.stringify({name:'mods-native-row-wire-289',version:'1.0.0',description:'Local row/UTF16 fixture',author:{name:'Local fixture'}})+'\n',
  'hooks/hooks.json': JSON.stringify({modules:['./register.js']})+'\n',
  'hooks/register.js': `
const observations = [];
export function register(on) {
  on('session.append', async ($, event, next) => {
    const before = JSON.parse(JSON.stringify(event));
    const units = text => Array.from({length:text.length},(_,i)=>text.charCodeAt(i));
    const textUnits = message => message.content.filter(b=>b.type==='text'&&typeof b.text==='string').map(b=>({text:b.text,units:units(b.text)}));
    const observation = {before, before_units:textUnits(event.message), frozen_input:{event:Object.isFrozen(event),message:Object.isFrozen(event.message),content:Object.isFrozen(event.message.content)}};
    observations.push(observation);
    await $.fs.write('append-observations.json', JSON.stringify(observations));
    let content = event.message.content;
    if (event.message.type === 'assistant') {
      content = content.map(block => block.type === 'text' && typeof block.text === 'string' && block.text.includes('NATIVE_ROW_FIRST_289')
        ? {...block, text: 'NATIVE_ROW_ACCEPTED_289_' + String.fromCharCode(0xd800) + 'Ω' + String.fromCharCode(0xdc00)}
        : block);
    }
    const nextEvent = {...event, message: {...event.message, content}};
    observation.forwarded = JSON.parse(JSON.stringify(nextEvent));
    observation.forwarded_units = textUnits(nextEvent.message);
    await $.fs.write('append-observations.json', JSON.stringify(observations));
    try {
      const result = await next(nextEvent);
      observation.result = JSON.parse(JSON.stringify(result));
      observation.result_units = textUnits(result.message);
      await $.fs.write('append-observations.json', JSON.stringify(observations));
      return result;
    } catch (error) {
      observation.error = String(error);
      await $.fs.write('append-observations.json', JSON.stringify(observations));
      throw error;
    }
  });
  on('turn.complete', async ($, event, next) => {
    let summary, api, error;
    try { summary = await $.session.messages(); api = await $.session.messages({as:'api'}); }
    catch (e) { error = String(e); }
    const units = text => Array.from({length:text.length},(_,i)=>text.charCodeAt(i));
    const summaryRows = (summary ?? []).filter(row=>typeof row.text==='string'&&row.text.startsWith('NATIVE_ROW_ACCEPTED_289_')).map(row=>({role:row.role,text:row.text,units:units(row.text)}));
    const apiBlocks = (api ?? []).flatMap(row=>row.content??[]).filter(block=>block.type==='text'&&typeof block.text==='string'&&block.text.startsWith('NATIVE_ROW_ACCEPTED_289_')).map(block=>({text:block.text,units:units(block.text),citations:block.citations}));
    const suffix = event.answer?.includes('NATIVE_ROW_RESUME_289') ? 'resume' : event.turnId;
    await $.fs.write('session-observations-' + suffix + '.json', JSON.stringify({answer:event.answer, summary:summaryRows, api:apiBlocks, error}));
    return next(event);
  });
}
`,
};
const fixtureHashes={};
for(const [relative,source] of Object.entries(fixture)) {
  await writeFile(path.join(pluginDir,relative),source);
  fixtureHashes[relative]=hash(source);
}
const dummy='sk-ant-local-row-wire-only';
const requests=[],health=[],unexpected=[];
const citation={type:'char_location',cited_text:'seed',document_index:0,document_title:'fixture',start_char_index:0,end_char_index:4};
function sendSse(response, body, turn) {
  response.writeHead(200,{'content-type':'text/event-stream','cache-control':'no-cache'});
  const send=(event,data)=>response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  send('message_start',{type:'message_start',message:{id:`msg_local_row_${turn}`,type:'message',role:'assistant',model:body.model,content:[],stop_reason:null,stop_sequence:null,usage:{input_tokens:1,output_tokens:0}}});
  const block={type:'text',text:''};
  if(turn===1) {block.citations=[citation];block.extra_native={kept:289};}
  send('content_block_start',{type:'content_block_start',index:0,content_block:block});
  send('content_block_delta',{type:'content_block_delta',index:0,delta:{type:'text_delta',text:turn===1?'NATIVE_ROW_FIRST_289_'+String.fromCharCode(0xd800)+'Ω'+String.fromCharCode(0xdc00):turn===2?'NATIVE_ROW_FINAL_289':'NATIVE_ROW_RESUME_289'}});
  send('content_block_stop',{type:'content_block_stop',index:0});
  send('message_delta',{type:'message_delta',delta:{stop_reason:'end_turn',stop_sequence:null},usage:{output_tokens:2}});
  send('message_stop',{type:'message_stop'});
  response.end();
}
const server=createServer(async(request,response)=>{
  if(request.socket.remoteAddress!=='127.0.0.1') {unexpected.push('peer');response.writeHead(403).end();return;}
  if(request.method==='HEAD'&&request.url==='/api/hello') {health.push('HEAD /api/hello');response.writeHead(200).end();return;}
  if(request.method!=='POST'||request.url!=='/v1/messages?beta=true') {unexpected.push({method:request.method,path:request.url});response.writeHead(404).end();return;}
  if(request.headers['x-api-key']!==dummy) {unexpected.push('auth');response.writeHead(401).end();return;}
  const chunks=[];for await(const chunk of request)chunks.push(chunk);
  const bytes=Buffer.concat(chunks);const body=JSON.parse(bytes.toString('utf8'));
  requests.push({method:request.method,path:request.url,auth_matches_dummy:true,body,body_json:bytes.toString('utf8'),sha256:hash(bytes)});
  if(requests.length>3) {unexpected.push('count');response.writeHead(429).end();return;}
  sendSse(response,body,requests.length);
});
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
const baseUrl=`http://127.0.0.1:${server.address().port}`;
const sessionId=randomUUID();
const argv=['--print','--verbose','--input-format','stream-json','--output-format','stream-json','--include-hook-events','--plugin-dir',pluginDir,'--session-id',sessionId,'--model','sonnet','--permission-mode','acceptEdits','--permission-prompts','none','--max-budget-usd','0.02'];
const env={PATH:'/usr/bin:/bin',TMPDIR:tempDir,LANG:'C.UTF-8',CLAUDE_CONFIG_DIR:configDir,ANTHROPIC_API_KEY:dummy,ANTHROPIC_BASE_URL:baseUrl,DISABLE_TELEMETRY:'1',DO_NOT_TRACK:'1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:'1',DISABLE_AUTOUPDATER:'1'};
const stdout=[],stderr=[];
const child=spawn(binaryPath,argv,{cwd:sessionDir,env,stdio:['pipe','pipe','pipe'],detached:true});
let lineBuffer='', resultCount=0;
function submit(text) {
  child.stdin.write(JSON.stringify({type:'user',session_id:sessionId,parent_tool_use_id:null,message:{role:'user',content:text}})+'\n');
}
child.stdout.on('data',b=>{
  stdout.push(b);lineBuffer+=b.toString('utf8');
  while(lineBuffer.includes('\n')) {
    const end=lineBuffer.indexOf('\n');const line=lineBuffer.slice(0,end);lineBuffer=lineBuffer.slice(end+1);
    let row;try{row=JSON.parse(line);}catch{continue;}
    if(row.type==='result') {
      resultCount++;
      if(resultCount===1) submit('Finish the second query with NATIVE_ROW_FINAL_289.');
      else child.stdin.end();
    }
  }
});
child.stderr.on('data',b=>stderr.push(b));
submit('Finish the first query with NATIVE_ROW_FIRST_289.');
const signal=s=>{try{process.kill(-child.pid,s);}catch{child.kill(s);}};
let timedOut=false,forcedKill;
const deadline=setTimeout(()=>{timedOut=true;signal('SIGTERM');forcedKill=setTimeout(()=>signal('SIGKILL'),3000);},60000);
let exit;
try{exit=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('close',(code,signal)=>resolve({code,signal}));});}
finally{clearTimeout(deadline);clearTimeout(forcedKill);try{process.kill(-child.pid,0);signal('SIGKILL');}catch{}}
const firstAppend = await readJson(path.join(sessionDir,'append-observations.json'));
const resumeArgv=[...argv];
const sessionArgIndex=resumeArgv.indexOf('--session-id');
assert.notEqual(sessionArgIndex,-1,'initial launch pins a session id');
resumeArgv.splice(sessionArgIndex,2);
resumeArgv.push('--resume',sessionId);
const resumed=spawn(binaryPath,resumeArgv,{cwd:sessionDir,env,stdio:['pipe','pipe','pipe'],detached:true});
let resumeLineBuffer='',resumeResultCount=0;
resumed.stdout.on('data',b=>{
  stdout.push(b);resumeLineBuffer+=b.toString('utf8');
  while(resumeLineBuffer.includes('\n')) {
    const end=resumeLineBuffer.indexOf('\n');const line=resumeLineBuffer.slice(0,end);resumeLineBuffer=resumeLineBuffer.slice(end+1);
    let row;try{row=JSON.parse(line);}catch{continue;}
    if(row.type==='result') {resumeResultCount++;if(resumeResultCount===1)resumed.stdin.end();}
  }
});
resumed.stderr.on('data',b=>stderr.push(b));
resumed.stdin.write(JSON.stringify({type:'user',session_id:sessionId,parent_tool_use_id:null,message:{role:'user',content:'Resume the saved conversation and finish with NATIVE_ROW_RESUME_289.'}})+'\n');
let resumeTimedOut=false,resumeForcedKill;
const resumeDeadline=setTimeout(()=>{resumeTimedOut=true;try{process.kill(-resumed.pid,'SIGTERM');}catch{resumed.kill('SIGTERM');}resumeForcedKill=setTimeout(()=>{try{process.kill(-resumed.pid,'SIGKILL');}catch{resumed.kill('SIGKILL');}},3000);},60000);
let resumeExit;
try{resumeExit=await new Promise((resolve,reject)=>{resumed.once('error',reject);resumed.once('close',(code,signal)=>resolve({code,signal}));});}
finally{clearTimeout(resumeDeadline);clearTimeout(resumeForcedKill);try{process.kill(-resumed.pid,0);process.kill(-resumed.pid,'SIGKILL');}catch{}}
server.closeAllConnections();await new Promise(r=>server.close(r));
const out=Buffer.concat(stdout),err=Buffer.concat(stderr);
await writeFile(path.join(root,'stdout.bin'),out);await writeFile(path.join(root,'stderr.bin'),err);
async function readJson(file){try{return JSON.parse(await readFile(file,'utf8'));}catch{return null;}}
const append=await readJson(path.join(sessionDir,'append-observations.json'));
const appendRows=[...(firstAppend??[]),...(append??[])];
const session=await readJson(path.join(sessionDir,'session-observations-resume.json'));
const exact='NATIVE_ROW_ACCEPTED_289_'+String.fromCharCode(0xd800)+'Ω'+String.fromCharCode(0xdc00);
const expected='NATIVE_ROW_ACCEPTED_289_'+String.fromCharCode(0xfffd)+'Ω'+String.fromCharCode(0xfffd);
const sourceExact='NATIVE_ROW_FIRST_289_'+String.fromCharCode(0xd800)+'Ω'+String.fromCharCode(0xdc00);
const expectedUnits=Array.from({length:expected.length},(_,i)=>expected.charCodeAt(i));
const liveSessions=await Promise.all((await readdir(sessionDir)).filter(name=>name.startsWith('session-observations-')&&!name.endsWith('-resume.json')).map(name=>readJson(path.join(sessionDir,name))));
const liveSession=liveSessions.find(observation=>observation?.summary?.some(row=>row.text===exact)&&observation?.api?.some(block=>block.text===exact));
const jsonl=[];
async function collect(dir){for(const item of await readdir(dir,{withFileTypes:true})){const file=path.join(dir,item.name);if(item.isDirectory())await collect(file);else if(item.name.endsWith('.jsonl')){const b=await readFile(file);const raw=b.toString('utf8');jsonl.push({path:file,sha256:hash(b),bytes:b.length,raw,rows:raw.trim().split('\n').filter(Boolean).map(s=>JSON.parse(s))});}}}
await collect(configDir);
const requestText=requests[1]?.body.messages.flatMap(m=>m.content??[]).find(b=>b.type==='text'&&typeof b.text==='string'&&b.text.startsWith('NATIVE_ROW_ACCEPTED_289_'));
const resumedRequestText=requests[2]?.body.messages.flatMap(m=>m.content??[]).find(b=>b.type==='text'&&typeof b.text==='string'&&b.text.startsWith('NATIVE_ROW_ACCEPTED_289_'));
const sourceText=appendRows.flatMap(o=>o.before.message.content).find(b=>b.type==='text'&&typeof b.text==='string'&&b.text.startsWith('NATIVE_ROW_FIRST_289'));
const acceptedBlock=message=>message.content?.find(b=>b.type==='text'&&b.text===exact);
const rewritten=appendRows.find(o=>o.before.message.type==='assistant'&&o.before_units.some(b=>b.text.startsWith('NATIVE_ROW_FIRST_289')));
const durableBlock=jsonl.flatMap(file=>file.rows).filter(row=>row.type==='assistant').map(row=>acceptedBlock(row.message)).find(Boolean);
const apiBlock=liveSession?.api?.find(block=>block.text===exact);
const summaryBlock=liveSession?.summary?.find(row=>row.text===exact);
const rawJsonl=jsonl.map(file=>file.raw).join('\n');
const escapedExact=JSON.stringify(exact).slice(1,-1);
const normalizedWireMatches=requestText?.text===expected&&requestText?.citations===null&&Object.keys(requestText??{}).length===3;
const resumedWireMatches=resumedRequestText?.text===expected&&resumedRequestText?.citations===null&&Object.keys(resumedRequestText??{}).length===3;
const beforeUnitsMatch=rewritten?.before_units.some(b=>b.text===sourceExact&&b.units.includes(55296)&&b.units.includes(56320)&&b.units.includes(937));
const hookUnitsMatch=beforeUnitsMatch&&rewritten?.forwarded_units.some(b=>b.text===exact&&b.units.includes(55296)&&b.units.includes(56320)&&b.units.includes(937))&&rewritten?.result_units.some(b=>b.text===exact&&b.units.includes(55296)&&b.units.includes(56320)&&b.units.includes(937));
const durableUnitsMatch=durableBlock?.text===exact&&durableBlock?.citations===null&&Object.keys(durableBlock??{}).length===3;
const apiUnitsMatch=apiBlock?.text===exact&&summaryBlock?.text===exact&&apiBlock?.units.includes(55296)&&apiBlock?.units.includes(56320)&&summaryBlock?.units.includes(937);
const resumedRow=session?.summary?.find(row=>row.text.startsWith('NATIVE_ROW_ACCEPTED_289_'));
const resumedApiBlock=session?.api?.find(block=>block.text.startsWith('NATIVE_ROW_ACCEPTED_289_'));
const resumeReloadObserved=session?.answer==='NATIVE_ROW_RESUME_289'&&resumedRow?.text===expected&&resumedApiBlock?.text===expected&&JSON.stringify(resumedRow.units)===JSON.stringify(expectedUnits)&&JSON.stringify(resumedApiBlock.units)===JSON.stringify(expectedUnits);
const durableJsonlStandard=rawJsonl.includes(escapedExact)&&!rawJsonl.includes('__lingxiModUtf16StringsV1')&&!rawJsonl.includes('utf16_code_units');
const resumedHistoryExact=session?.answer==='NATIVE_ROW_RESUME_289'&&apiUnitsMatch;
const acceptance={passed:exit.code===0&&resumeExit.code===0&&!timedOut&&!resumeTimedOut&&unexpected.length===0&&requests.length===3&&health.length>=1&&normalizedWireMatches&&resumedWireMatches&&hookUnitsMatch&&durableUnitsMatch&&durableJsonlStandard&&apiUnitsMatch&&resumeReloadObserved&&out.toString('utf8').includes('NATIVE_ROW_RESUME_289'),request_count:requests.length,source_text:sourceText??null,next_request_text:requestText??null,next_request_units:requestText?Array.from({length:requestText.text.length},(_,i)=>requestText.text.charCodeAt(i)):null,resume_request_text:resumedRequestText??null,resume_request_units:resumedRequestText?Array.from({length:resumedRequestText.text.length},(_,i)=>resumedRequestText.text.charCodeAt(i)):null,before_forwarded_result_units_match:hookUnitsMatch,durable_units_match:durableUnitsMatch,durable_jsonl_standard_text_escape:durableJsonlStandard,live_session_api_units_match:apiUnitsMatch,resume_reload_normalizes_to_replacement:resumeReloadObserved,normalized_http_wire_matches:normalizedWireMatches,resume_http_wire_matches:resumedWireMatches};
const receipt={root,binary_sha256:hash(binary),argv,resume_argv:resumeArgv,fixture_hashes:fixtureHashes,constructed_environment:{inherited:false,home_set:false,credentials:'fixed dummy only; not serialized',base_url:baseUrl},exit,resume_exit:resumeExit,timed_out:timedOut||resumeTimedOut,local_provider:{request_count:requests.length,health,unexpected,relayed_externally:false},requests,append:appendRows,live_session_messages:liveSession,resumed_session_messages:session,jsonl,stdout:{bytes:out.length,sha256:hash(out),path:path.join(root,'stdout.bin')},stderr:{bytes:err.length,sha256:hash(err),path:path.join(root,'stderr.bin')},acceptance,scope:'Actual Native2.1.289 enabled-Mod exact UTF-16 Text rewrite, before/forwarded/result, standard escaped JSONL with no carrier leak, second-process --resume, and separate live-versus-reloaded session.messages summary/API observations. This binary preserves exact units in live Mod/API and raw JSONL, but this runner observes replacement units from session.messages after Native process resume. Downstream HTTP replacement is a separate local observation; this does not establish public SDK byte capability or Harness parity.'};
await writeFile(path.join(root,'receipt.json'),JSON.stringify(receipt,null,2)+'\n');
console.log(JSON.stringify({root,receipt:path.join(root,'receipt.json'),exit,acceptance},null,2));
process.exitCode=acceptance.passed?0:1;
