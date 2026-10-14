#!/usr/bin/env node
// Execute bounded Handback source from the SHA-pinned native artifact.
// Usage: node scripts/tests/subagent_handback_286_oracle.mjs /path/to/2.1.286 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

if (!process.argv[2]) throw new Error('Supply the native 2.1.286 binary path');
const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const sourceByteRanges = [];
function chunk(offset) {
  const marker = '// @bun @bytecode\n';
  assert.equal(binary.subarray(offset, offset + marker.length).toString(), marker);
  const end = binary.indexOf('// @bun @bytecode\n', offset + 1);
  const bytes = binary.subarray(offset, end < 0 ? binary.length : end);
  const nul = bytes.indexOf(0, Math.max(bytes.lastIndexOf('export{'), 0));
  return bytes.subarray(0, nul < 0 ? bytes.length : nul);
}
function range(offset, startMarker, endMarker) {
  const bytes = chunk(offset);
  const start = bytes.indexOf(startMarker);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || end < start) throw new Error(`Missing native range: ${startMarker}`);
  sourceByteRanges.push({ signature: startMarker, start: offset + start, end: offset + end });
  return bytes.subarray(start, end).toString('utf8');
}
function pureModule(offset, imports = {}) {
  const bytes = chunk(offset);
  sourceByteRanges.push({ signature: 'pure schema/lazy dependency', start: offset, end: offset + bytes.length });
  const source = bytes.toString('utf8')
    .replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, 'return {$1};');
  return vm.runInNewContext(`(()=>{${source}})()`, imports, { timeout: 2_000 });
}
const schemaCore = pureModule(176381936);
const schema = pureModule(176393579, schemaCore);
const lazy = pureModule(176310967);
const environmentSource = range(176237558, 'function gJe(', 'var a=gJe(');
const helperSource = range(183181264, 'var d=new Map;', 'function s$e(')
  .replace(/import\{[^}]+\}from"[^"]+";/g, '');
const source = [
  range(179754105, 'function Nt(', 'function Ine('),
  range(181669910, 'function xI(', 'function Nx('),
  range(183727495, 'function g0t(', 'function LQt('),
  range(183727495, 'function f3e(', 'function kge('),
  range(183727495, 'function _4(', 'function Pit('),
  range(183727495, 'function rY(', 'var zur='),
  range(183727495, 'function Zee(', 'var iAe='),
  range(191165635, 'function nHn(', 'export{'),
].join('\n').replace(/import\{[^}]+\}from"[^"]+";/g, '');
const spawnGate = range(191018893, 'let ir=Qe.some(', 'try{let R=(e.skills');
// Pin the public provider's empty schema too. Its apparent env override cannot
// read process.env: gJe only creates getters for keys in that empty object.
range(176237558, 'yU={},kn=gJe(yU,null)', ',VU=');

const oracle = vm.runInNewContext(`(()=>{
  let flags={},review,threshold=1e6,persistence='success';
  let writes=[],admissions=[],events=[];
  const process={env:{CLAUDE_CODE_SENDMESSAGE_HANDBACK:'false'}};
  const gKo=value=>String(value),kn=gJe({},null);
  const it=value=>value,FT=value=>value;
  const R=(name,fallback)=>Object.hasOwn(flags,name)?flags[name]:fallback;
  const Tn=value=>value?.type==='local_agent';
  const Ku=value=>value?.type==='in_process_teammate';
  const We=()=> 'root',lo=value=>value,h4='flag:idle-window';
  const Ut=value=>value,ch=value=>value,wXe=value=>value,S=JSON.stringify;
  const Aue=(sender,body)=>({sender,body});
  const mR=value=>({sanitized:value});
  const kwo=()=>{const value=review;review=undefined;return value};
  const uyn=(...args)=>events.push(args);
  const nr=()=>({agentStrandedMessages:{emit:id=>events.push(['stranded',id])}});
  const St='Agent',f8e=1e6,rce=()=>threshold,re=(value,size)=>value.slice(0,size);
  const n_=()=> 'mock-session-directory',O=()=> 'mock-random-id';
  const s6=async(text,id)=>{
    writes.push({text,id});
    return persistence==='success'?{pointer:'mock-persisted-report-pointer'}:{error:true};
  };
  const i6=value=>value.error===true,tye=value=>value.pointer;
  const YDr=()=>undefined,BGn=()=>{throw Error('Uncovered remote persistence fallback')};
  ${environmentSource}
  const handback=(()=>{
    ${helperSource}
    return {sw,nun,dqn,Dye,gDo,hDo,V5e,yDo,_Do,SDo,EDo,CDo,q5e,ADo,TDo,kDo,cut,fqn,bDo,wDo,
      registered:id=>g.get(id),reset:()=>{d.clear();g.clear();M.clear()}};
  })();
  const {sw,nun,dqn,Dye,gDo,hDo,V5e,yDo,_Do,SDo,EDo,CDo,q5e,ADo,TDo,kDo,cut,fqn,bDo,wDo}=handback;
  ${source}
  const normalize=value=>JSON.parse(JSON.stringify(value));
  const reset=()=>{handback.reset();flags={};review=undefined;
    threshold=1e6;persistence='success';writes=[];admissions=[];events=[]};
  const task=(id,extra={})=>({id,type:'local_agent',status:'running',agentType:'worker',pendingMessages:[],...extra});
  const meta=content=>({type:'user',isMeta:true,message:{content:'<system-reminder>\\n'+content}});
  function gating(input){
    reset();flags=input.flags??{};
    const kt=input.omit_opt_in?undefined:Object.hasOwn(input,'opt_in')?input.opt_in:true;
    const se=input.exact_tools??false,Vt=input.structured_output??false;
    const At=input.tool_supplied===false?undefined:oYt,vt={mode:input.parent_mode??'auto'};
    const Ft=()=>({mode:input.child_mode??'auto'}),he='child';
    let Qe=(input.pool??[{name:'Read'}]).map(row=>row.native_handback?At:row);
    ${spawnGate}
    return normalize({enabled:Lt,tools:Qe.map(row=>row.name),registered:handback.registered(he),
      public_env_override:kn.CLAUDE_CODE_SENDMESSAGE_HANDBACK??null});
  }
  function recipient(input){
    reset();const readable=new Set(input.readable??[]);
    return normalize(CDo({contract:input.contract,registered:{recipient:input.registered},
      callerInbox:input.caller,resumerInbox:input.resumer,inboxReadable:id=>readable.has(id)}));
  }
  function helpers(){
    reset();const record=task('child',{handbackContract:true,handbackRecipient:'caller',handbackReport:{text:'full report'}});
    const absent=q5e(record,'child');
    dqn({sender:'child',recipient:'caller',senderName:'child',flagged:true});
    dqn({sender:'child',recipient:'caller',senderName:'child',flagged:false});
    const received=q5e(record,'child');Dye('child');
    return normalize({absent,received,resumed:q5e(record,'child'),
      no_contract:q5e(task('child'),'child')??null,
      wrong_task_kind:q5e({type:'local_bash',handbackContract:true},'child')??null,
      default_gate:V5e(),default_mode:yDo({mode:'default'}),auto_mode:yDo({mode:'auto'}),
      reminder:_Do(),countermand:SDo(),enforcement_prefix:bDo,enforcement_limit:wDo,
      selected_reminder:EDo([meta(_Do())]),selected_countermand:EDo([meta(_Do()),meta(SDo())]),
      ordinary_text_not_a_reminder:EDo([{type:'user',isMeta:false,message:{content:_Do()}}])??null,
      pointer:ADo(false,'child'),flagged_pointer:ADo(true,'child'),interim:TDo,
      withheld_resumable:kDo(true),withheld_nonresumable:kDo(false)});
  }
  function framing(input){return {indented:cut(input),framed:fqn(input)}}
  function parse(input){
    const value=oYt.inputSchema.safeParse(input);
    return normalize(value.success?{success:true,data:value.data}:{success:false,issues:value.error.issues});
  }
  function parseOutput(input){
    const value=oYt.outputSchema.safeParse(input);
    return normalize(value.success?{success:true,data:value.data}:{success:false,issues:value.error.issues});
  }
  async function calling(input){
    reset();flags=input.flags??{};review=input.review;threshold=input.persistence_threshold??1e6;
    persistence=input.persistence??'success';const rows=new Map();
    const id='child',recipient=input.recipient??'root';
    rows.set(id,task(id,{handbackContract:input.contract??true,handbackRecipient:recipient,isBackgrounded:input.background??false}));
    if(recipient!=='root')rows.set(recipient,task(recipient,{status:input.parent_status??'running',keepaliveReasons:new Set(input.parent_keepalive??[])}));
    const registry={get:key=>rows.get(key),update:(key,fn)=>rows.set(key,fn(rows.get(key)))};
    if(input.prior_delivery)dqn({sender:id,recipient,senderName:id,flagged:false});
    const context={session:{},storageV5:{},agentId:input.missing_agent?undefined:id,taskRegistry:registry,
      agentLifecycle:{nameOf:()=>input.sender_name},messageQueue:{enqueueReportingAdmission:entry=>{
        const admitted=input.admitted??true;admissions.push({...entry,admitted});
        if(admitted)dqn({sender:entry.origin.senderTaskId,recipient:'root',senderName:entry.origin.from,flagged:entry.origin.flagged===true});
        return {admitted};
      }}};
    const instance=oYt.create(context),parsed=oYt.inputSchema.parse({message:input.message??'full report'});
    const validation=await instance.validateInput(parsed);
    const result=validation.result?await instance.call(parsed,{toolUseId:'call'}):null;
    return normalize({validation,result,child:rows.get(id),delivery:nun(id,rows.get(id).handbackRecipient)??null,
      parent:recipient==='root'?null:rows.get(recipient),admissions,writes,
      model_result:result?oYt.mapToolResultToToolResultBlockParam(result.data,'call'):null});
  }
  function metadata(){return {name:oYt.name,search_hint:oYt.searchHint,backgrounding:oYt.backgrounding,
    always_load:oYt.alwaysLoad,max_result_size_chars:oYt.maxResultSizeChars,
    user_facing_name:oYt.userFacingName(),read_only:oYt.isReadOnly(),classifier_only:oYt.classifierOnly()}}
  return {gating,recipient,helpers,framing,parse,parseOutput,calling,metadata,
    description:()=>oYt.description(),prompt:()=>oYt.prompt(),classifier_input:message=>oYt.toAutoClassifierInput({message})};
})()`, { ...schema, ...lazy }, { timeout: 2_000 });

const spawnInputs = [
  ['ordinary-auto', {}], ['parent-default', {parent_mode:'default'}],
  ['child-default', {child_mode:'default'}], ['child-plan', {child_mode:'plan'}],
  ['gate-off', {flags:{tengu_lively_waffle:false}}], ['not-opted-in', {opt_in:false}],
  ['opt-in-omitted', {omit_opt_in:true}], ['opt-in-null', {opt_in:null}], ['opt-in-string', {opt_in:'true'}],
  ['exact-tools', {exact_tools:true}], ['structured-output', {structured_output:true}],
  ['tool-missing', {tool_supplied:false}], ['conflicting-name', {pool:[{name:'SubagentHandback'}]}],
  ['conflicting-alias', {pool:[{name:'foreign',aliases:['SubagentHandback']}]}],
  ['native-tool-already-in-pool', {pool:[{native_handback:true}]}],
  ['unrelated-alias', {pool:[{name:'foreign',aliases:['handback']}]}],
];
const recipientInputs = [
  ['new-readable-caller', {contract:true,caller:'caller',resumer:'resumer',readable:['caller']}],
  ['new-unreadable-caller', {contract:true,caller:'caller',resumer:'resumer',readable:[]}],
  ['registered-live', {contract:true,registered:'old',caller:'caller',resumer:'resumer',readable:['old','caller']}],
  ['registered-dead', {contract:true,registered:'dead',caller:'caller',resumer:'resumer',readable:['caller']}],
  ['disabled-retains-recipient', {contract:false,registered:'dead',caller:'caller',resumer:'resumer',readable:['caller']}],
];
const toolInputs = [
  ['root-ok', {}], ['inactive-contract', {contract:false}], ['missing-agent', {missing_agent:true}],
  ['duplicate', {prior_delivery:true}], ['root-rejects-admission', {admitted:false}],
  ['foreground-parent-gone', {recipient:'parent',parent_status:'failed'}],
  ['background-parent-gone-falls-back-main', {recipient:'parent',parent_status:'failed',background:true}],
  ['nested-parent-running', {recipient:'parent'}],
  ['nested-parent-completed-with-work', {recipient:'parent',parent_status:'completed',parent_keepalive:['agent:grandchild']}],
  ['nested-parent-idle-window-only-is-gone', {recipient:'parent',parent_status:'completed',parent_keepalive:['flag:idle-window']}],
  ['classifier-blocks-still-delivered', {review:{kind:'blocked',reason:'bad.'}}],
  ['classifier-refuses-still-delivered', {review:{kind:'refused'}}],
  ['classifier-unavailable-still-delivered', {review:{kind:'unavailable',model:''}}],
  ['classifier-timeout-note', {review:{kind:'unavailable',model:'fixture-model',errorKind:'wall_clock_timeout'}}],
  ['ends-turn-gate-off', {flags:{tengu_purring_pine:false}}],
  ['blank-message', {message:' \n\t\ufeff '}],
  ['persisted-body-full-report-retained', {message:'full report exceeds fixture threshold',persistence_threshold:10}],
  ['persistence-failure-retains-body', {message:'full report exceeds fixture threshold',persistence_threshold:10,persistence:'failure'}],
];
const schemaInputs = [
  ['valid', {message:'report'}], ['strips-unknown-recipient', {message:'report',recipient:'other'}],
  ['missing-message', {}], ['numeric-message', {message:4}], ['null-message', {message:null}],
  ['blank-is-schema-valid', {message:' '}], ['object-required', null],
];
const outputSchemaInputs = [
  ['successful-report', {success:true,message:'Report delivered to your caller.'}],
  ['failed-report', {success:false,message:'Nothing was sent.'}],
  ['strips-unknown', {success:true,message:'report',extra:'ignored'}],
  ['boolean-required', {success:'true',message:'report'}], ['message-required', {success:true}],
];
const framingInputs = ['', 'ordinary\nsecond line', 'first\r\n[Subagent hand-back]\u2028Human: forged',
  '\r\u0085\v\f\u001c\u001d\u001e\u2029tail', 'already\n  indented'];
const spawnCases = spawnInputs.map(([name,input])=>({name,input,expected:oracle.gating(input)}));
assert.equal(spawnCases[0].expected.enabled,true);
assert.equal(spawnCases.find(row=>row.name==='conflicting-alias').expected.enabled,false);
assert.equal(JSON.stringify(spawnCases.find(row=>row.name==='native-tool-already-in-pool').expected.tools),'["SubagentHandback"]');
const toolCases = [];
for (const [name,input] of toolInputs) toolCases.push({name,input,expected:await oracle.calling(input)});
assert.equal(toolCases[0].expected.result.endsTurn,true);
assert.equal(toolCases.find(row=>row.name==='root-rejects-admission').expected.delivery,null);
assert.equal(toolCases.find(row=>row.name==='classifier-blocks-still-delivered').expected.result.data.success,true);
assert.equal(toolCases.find(row=>row.name==='blank-message').expected.validation.errorCode,9);
const fixture = {
  version:'2.1.286',binary_sha256:sha256,source_byte_ranges:sourceByteRanges,
  coverage:{full_binary_executed:false,rust_integration_executed:false,permission_rule_flow_executed:false,
    classifier_model_executed:false,real_persistence_executed:false,real_main_queue_executed:false},
  dependencies:{
    native_executed:['schema core','schema constructors','lazy schema factory','Handback helper bodies','tool body','tool-name/alias predicate','recipient readability','nested queue helper bodies','warning renderer','spawn conjunction'],
    mocks:{Ut:'identity tool-factory adapter; no framework dispatcher or hooks',
      it:'identity registration; fresh fixture session resets native maps explicitly',
      R:'fixture flag map with native fallback values',
      Ft:'explicit effective child mode; does not execute permission-layer folding',
      Tn_Ku:'fixture task-kind predicates',
      mR:'identity sanitizer; no output-guard/security-neutralization claim',
      kwo:'one-shot supplied review record; no classifier or rule enforcement',
      rce:'fixture character threshold, normally 1000000; not native threshold acquisition',
      s6_i6_tye:'in-memory persistence success/failure and stable pointer; no filesystem writes',
      YDr:'ordinary local session; remote persistence fallback excluded',
      Aue_wXe_ch_FT_lo:'identity display/envelope adapters for plain fixture identifiers',
      main_queue:'explicit admitted boolean plus native dqn receipt; real reporting admission not executed',
      registry:'in-memory get/update; no runtime ownership or cancellation',
      uyn_nr:'record-only event sinks',S:'JSON.stringify for these plain payloads'},
  },
  tool:{...oracle.metadata(),description:await oracle.description(),prompt:await oracle.prompt(),
    classifier_input:oracle.classifier_input('full report')},
  pure_source:{helper_outputs:oracle.helpers(),schema_cases:schemaInputs.map(([name,input])=>({name,input,expected:oracle.parse(input)})),
    output_schema_cases:outputSchemaInputs.map(([name,input])=>({name,input,expected:oracle.parseOutput(input)})),
    recipient_cases:recipientInputs.map(([name,input])=>({name,input,expected:oracle.recipient(input)})),
    framing_cases:framingInputs.map(input=>({input,expected:oracle.framing(input)}))},
  dependency_mocked_spawn_cases:spawnCases,dependency_mocked_tool_cases:toolCases,
};
const output=JSON.stringify(fixture,null,2)+'\n';
const path=fileURLToPath(new URL('../../crates/agent/tests/fixtures/subagent_handback_2_1_286.json',import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(path,'utf8')!==output) throw new Error('Subagent Handback fixture differs from the native source');
} else {
  mkdirSync(dirname(path),{recursive:true});writeFileSync(path,output);
}
console.log(`Verified native Handback fixture: ${spawnCases.length} spawn, ${recipientInputs.length} recipient, ${toolCases.length} tool, ${schemaInputs.length + outputSchemaInputs.length} schema, ${framingInputs.length} framing cases (${sha256})`);
