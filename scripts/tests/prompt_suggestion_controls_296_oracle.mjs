#!/usr/bin/env node
// Ordinary-tool source evidence: execute exact functions embedded in the pinned
// shipped binary. This is a local differential oracle, not a live API request.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binaryPath = process.env.CLAUDE_CODE_ORACLE_BINARY
  ?? '/Users/luolingfeng/.local/share/claude/versions/2.1.296';
const binary = readFileSync(binaryPath);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937');
function chunkAt(needle) {
  const at = binary.indexOf(needle, 180_000_000);
  assert(at >= 0, `missing native source: ${needle}`);
  const start = binary.lastIndexOf('// @bun @bytecode\n', at);
  const end = binary.indexOf(0, at);
  const bytes = binary.subarray(start, end);
  return { start, end, sha256: sha256(bytes), source: bytes.toString() };
}
const controls = chunkAt('var i=20,u=i+1,hKo=10');
const suggestions = chunkAt('function qIe(e){return e.status===');
const env = chunkAt('function Ne(e){if(!e)return!1;');
const input = chunkAt('function Ore({hasInput:');
const session = chunkAt('class we{abortController=null;eligibleTurns=0');
function span(source, from, to) {
  const start = source.indexOf(from);
  const end = source.indexOf(to, start);
  assert(start >= 0 && end > start, from);
  return source.slice(start, end);
}
const controlsSource = span(controls.source, 'function E_t()', '\nexport{');
const envSource = span(env.source, 'function Ne(e)', 'function ex(e)');
const gateSource = span(suggestions.source, 'function qIe(e)', 'async function Y$o(')
  + span(suggestions.source, 'var C=1e4;', 'var M=');
const dispatchSource = span(suggestions.source, 'async function Y$o(', 'async function v(');
const presentationSource = span(input.source, 'function Ore({hasInput:', 'N();function JI(');
const sessionSource = span(session.source, 'class we{abortController=null;eligibleTurns=0', 'class Le{');
const native = vm.createContext({
  a: {}, flags: {}, settings: {}, nonInteractive: false, teammate: false,
  status: 'allowed', events: [], i() {}, S: x => x, k(name, fallback) { return this.flags?.[name] ?? fallback; },
});
// Function globals are deliberately plain bindings rather than VM-host `this`.
vm.runInContext(`
  var a={},flags={},settings={},nonInteractive=false,teammate=false,status='allowed',events=[];
  function i(){} function S(x){return x} function k(n,d){return flags[n]??d}
  function ve(){return nonInteractive} function Jo(){return teammate} function Oc(){return teammate}
  function ft(){return settings} function qi(){return {status}}
  function F(items,p){return items.filter(p).length} function Ry(items){return items.findLast(x=>x.type==='assistant')}
  function ZH(reason,source,extras){events.push({reason,...extras})}
  function $Jn(){return false} var hKo=10;
  async function N(){return {suggestion:'run tests',generationRequestId:'req_native'}}
  function q(){return false}
  ${envSource}
  ${gateSource}
  function parseEnv(value){if(Ne(value))return true;if(Es(value))return false;return null}
`, native);
const backoff = vm.createContext({});
vm.runInContext(`
  var config={},a={},updates=[],events=[],now=2000000000000;
  var Date={now:()=>now,parse:globalThis.Date.parse};
  function ce(){return config} function Ce(fn){config=fn(config);updates.push(config)}
  async function uH(fn){Ce(fn);return true}
  function g(name,fields){events.push({name,fields})} function f(){} function t(){} function c(){} function G(x){return x}
  ${controlsSource}
  ${sessionSource}
  globalThis.SuggestionSession=we;
`, backoff);
const presentation = vm.createContext({});
vm.runInContext(`
  var state={promptSuggestion:{status:'empty'}},now=100,events=[],focused=true;
  var Date={now:()=>now}; function V(fn){return fn(state)} function Mn(){return fn=>{state=fn(state)}}
  function Qc(){return focused} function R(v){return {current:v}} function le(fn){return fn}
  function i(name,value){events.push(value)} function S(x){return x} function ye(x){return x}
  function qIe(e){return e.status==='shown'||e.status==='accepted'} function Wun(e){return e.status==='empty'?null:e.text}
  ${presentationSource}
`, presentation);
const focus = vm.createContext({AbortController});
vm.runInContext(`
  var currentFocus='unknown',backgroundMode=false,events=[],queryCalled=false,passedThrottle=false;
  function oX(){return currentFocus} function Nt(){return backgroundMode}
  function ZH(reason){events.push(reason)} function F5(){return {}}
  async function oNr(a,b,c,d,e,state){queryCalled=true;passedThrottle=state!==undefined;return null}
  function m(){} function c(){} function G(x){return x}
  ${dispatchSource}
`, focus);
const plain = x => JSON.parse(JSON.stringify(x));
const envValues = [null, '', 'true', 'TRUE', ' tRuE\t', '\ufeffOn\u3000', '\u0085true', 'yes', '1',
  'false', 'FALSE', '  No ', 'off', '0', 'enabled', '2', 'true\nfalse'];
const envCases = envValues.map(raw => ({raw, expected: native.parseEnv(raw ?? undefined)}));
const initialCases = [];
const explicitCases = [];
for (const env of [null, false, true]) for (const setting of [null, false, true]) {
  native.a = {CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: env ?? undefined};
  native.settings = {promptSuggestionEnabled: setting ?? undefined};
  explicitCases.push({env,setting,expected:native.zun()});
  for (const rollout of [false,true]) for (const nonInteractive of [false,true]) for (const teammate of [false,true]) {
    native.flags = {tengu_chomp_inflection:rollout};
    native.nonInteractive = nonInteractive;
    native.teammate = teammate;
    let event;
    native.i = (_, fields) => {event=fields;};
    const enabled = native.Gun();
    initialCases.push({env,setting,rollout,non_interactive:nonInteractive,swarm_teammate:teammate,
      expected:{enabled,source:event.source}});
  }
}
const gateInputs = [
  {}, {aborted:true}, {assistant_count:0}, {assistant_count:1}, {assistant_count:2},
  {last_response_api_error:true}, {enabled:false}, {pending_permission:true}, {elicitation_active:true}, {plan_mode:true},
  ...['allowed','allowed_warning','rejected','unknown',''].flatMap(rate_limit_status => [null,false,true].map(env=>({rate_limit_status,env}))),
  {aborted:true,assistant_count:0,enabled:false}, {assistant_count:1,last_response_api_error:true},
  {last_response_api_error:true,enabled:false}, {enabled:false,pending_permission:true},
  {pending_permission:true,elicitation_active:true}, {elicitation_active:true,plan_mode:true},
  {plan_mode:true,rate_limit_status:'rejected'},
  ...[[0,0,0],[9999,1,0],[10000,0,0],[10000,1,0],[9999,0,1],[9999,0,2],[0,0,10001],[10001,0,1]]
    .flatMap(([input_tokens,output_tokens,cache_creation_input_tokens])=>[false,true].map(cache_thaw=>({usage:{input_tokens,output_tokens,cache_creation_input_tokens},cache_thaw}))),
];
const gateCases=[];
for(const row of gateInputs){
  const value={aborted:false,assistant_count:2,last_response_api_error:false,enabled:true,pending_permission:false,
    elicitation_active:false,plan_mode:false,rate_limit_status:'allowed',env:null,usage:null,cache_thaw:false,...row};
  native.a={CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION:value.env??undefined};
  native.flags={tengu_chomp_thaw:value.cache_thaw};native.status=value.rate_limit_status;native.events=[];
  const messages=Array.from({length:value.assistant_count},()=>({type:'assistant',message:{usage:value.usage??{}}}));
  if(messages.length)messages.at(-1).isApiErrorMessage=value.last_response_api_error;
  const app={promptSuggestionEnabled:value.enabled,pendingWorkerRequest:value.pending_permission,
    mcp:{clients:[{type:'connected',transportErrorState:{pendingElicitations:value.elicitation_active?1:0}}]},
    toolPermissionContext:{mode:value.plan_mode?'plan':'default'}};
  await native.oNr({signal:{aborted:value.aborted}},messages,()=>app,{},'cli');
  gateCases.push({...value,expected:native.events[0]??null});
}
const backoffCases=[];
for(const streak of [null,'20',-1,0,19,19.999,20,20.001,21,22])
for(const env of [null,false,true])
for(const age of [null,-1,0,1_209_599_999,1_209_600_000,1_209_600_001,'invalid']){
  const firstStartTime=age===null?undefined:age==='invalid'?'invalid date':new globalThis.Date(backoff.now-age).toISOString();
  backoff.config={promptSuggestionUnusedStreak:streak,firstStartTime};
  backoff.a={CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION:env??undefined};
  backoff.updates=[];backoff.events=[];
  const expected=backoff.$Jn();
  const claim=backoff.XGr();
  backoffCases.push({streak,env,age,expected,notice_claimed:claim,
    notice_streak:backoff.config.promptSuggestionUnusedStreak});
}
const eligibleCases=[];
native.a={};native.flags={};native.status='allowed';native.hKo=backoff.hKo;
for(const backing_off of [false,true])for(const eligible_turns of [0,1,9,10,19,20]){
  native.$Jn=()=>backing_off;
  const state=new backoff.SuggestionSession();state.eligibleTurns=eligible_turns;state.unusedInARow=17;
  native.events=[];
  const result=await native.oNr({signal:{aborted:false}},[{type:'assistant',message:{usage:{}}},{type:'assistant',message:{usage:{}}}],
    ()=>({promptSuggestionEnabled:true,mcp:{clients:[]},toolPermissionContext:{mode:'default'}}),{},'cli',state);
  const after_eligible=state.eligibleTurns;
  state.askOnNextTurn();
  eligibleCases.push({backing_off,eligible_turns,expected:result!==null,after_eligible,
    after_ask:state.eligibleTurns,unused_after_ask:state.unusedInARow});
}
const submissionCases=[];
for(const accepted of [false,true])for(const streak of [null,0,19,20,21,22])for(const unused of [0,18,19,20]){
  backoff.config={promptSuggestionUnusedStreak:streak,firstStartTime:'1970-01-01'};
  backoff.a={};backoff.updates=[];backoff.events=[];
  const state={unusedInARow:unused,eligibleTurns:7};
  backoff.K2t(accepted,undefined,state);
  await Promise.resolve();
  submissionCases.push({accepted,streak,unused,expected_unused:state.unusedInARow,
    update:backoff.updates.length?backoff.config.promptSuggestionUnusedStreak:null});
}
const reenableCases=[];
for(const streak of [null,0,19,20,21,22]){
  backoff.config={promptSuggestionUnusedStreak:streak,firstStartTime:'1970-01-01'};backoff.a={};backoff.updates=[];
  const state={unusedInARow:19,eligibleTurns:7};backoff.yKo(undefined,state);await Promise.resolve();
  reenableCases.push({streak,expected_unused:state.unusedInARow,
    update:backoff.updates.length?backoff.config.promptSuggestionUnusedStreak:null});
}
const presentationCases=[];
for(const initial of ['empty','generated','shown','accepted'])
for(const input of ['run tests','RUN TESTS','different input',''])
for(const acceptedAt of [100,101]){
  const state=initial==='empty'?{status:'empty'}:{status:initial,text:'run tests',generationRequestId:'req_native',
    ...['shown','accepted'].includes(initial)&&{shownAt:100},...initial==='accepted'&&{acceptedAt}};
  presentation.state={promptSuggestion:state};presentation.now=200;presentation.events=[];
  const hook=presentation.Ore({hasInput:input!=='',isAssistantResponding:false});
  const accepted=hook.logOutcomeAtSubmission(input);
  const event=presentation.events[0];
  presentationCases.push({state,input,now:200,expected:accepted===null?null:{accepted,
    acceptMethod:event.acceptMethod??null,elapsedMs:event.timeToAcceptMs??event.timeToIgnoreMs,
    similarity:event.similarity,generationRequestId:event.generationRequestId??null},
    after:plain(presentation.state.promptSuggestion)});
}
const transitionCases=[];
for(const actions of [[],['shown'],['accepted'],['shown','shown'],['shown','accepted'],
  ['shown','accepted','shown'],['shown','accepted','accepted']]){
  presentation.state={promptSuggestion:{status:'generated',text:'run tests',generationRequestId:null}};
  presentation.now=100;
  for(const action of actions){
    const hook=presentation.Ore({hasInput:false,isAssistantResponding:false});
    if(action==='shown')hook.markShown();else hook.markAccepted();
    presentation.now++;
  }
  transitionCases.push({actions,expected:plain(presentation.state.promptSuggestion)});
}
const focusCases=[];
for(const query_source of [null,'repl_main_thread','repl_main_thread:agent','sdk','prompt_suggestion'])
for(const current_focus of ['unknown','focused','blurred'])
for(const background_mode of [false,true])for(const background_blocked_without_block of [false,true]){
  focus.currentFocus=current_focus;focus.backgroundMode=background_mode;focus.events=[];focus.queryCalled=false;focus.passedThrottle=false;
  const context={querySource:query_source,toolUseContext:{session:{promptSuggestion:{}},getAppState:()=>({})},messages:[]};
  await focus.Y$o(context,background_blocked_without_block?{tempo:'blocked'}:undefined);
  focusCases.push({query_source,current_focus,background_mode,background_blocked_without_block,
    expected:focus.events[0]??null,query_called:focus.queryCalled,passed_throttle:focus.passedThrottle});
}
const fixture={version:'2.1.296',binary_sha256:sha256(binary),evidence:'shipped native source, executed in Node VM with dependency stubs',
  sources:Object.fromEntries(Object.entries({controls,suggestions,env,input,session}).map(([name,{start,end,sha256}])=>[name,{start,end,sha256}])),
  envCases,initialCases,explicitCases,gateCases,backoffCases,eligibleCases,submissionCases,reenableCases,presentationCases,transitionCases,focusCases};
const output=fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/prompt_suggestion_controls_2_1_296.json',import.meta.url));
const json=JSON.stringify(fixture,null,2)+'\n';
if(process.argv.includes('--check')){
  if(readFileSync(output,'utf8')!==json)throw new Error('Native suggestion control fixture differs');
} else writeFileSync(output,json);
console.log(`Native suggestion controls: ${envCases.length} env, ${initialCases.length} initialization, ${explicitCases.length} explicit, ${gateCases.length} gate, ${backoffCases.length} backoff, ${eligibleCases.length} eligible turn, ${submissionCases.length} outcome, ${reenableCases.length} toggle, ${presentationCases.length} presentation, ${transitionCases.length} transition, ${focusCases.length} focus cases.`);
