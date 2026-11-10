// Replay unchanged, hash-pinned hook helpers from the installed 2.1.322 binary.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--write'));
const binary = readFileSync(args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322'));
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const ranges = [
  {name:'expand', start:274544022, end:274544540, sha256:'8250e16bb7019ef79782f2c991cd7f878a2190dc51d97595dc81e02210e864b7'},
  {name:'provenance', start:267958047, end:267958226, sha256:'0ad085f489f47119bf703b055496c3053f864b5a6b73dc69936133a0eb1e3372'},
  {name:'aliases',start:260524923,end:260525179,sha256:'25978d4c319fce41eba5ea8ad98b1362a7691c918861f7281940961c2dadf969'},
  {name:'rewriteFold',start:274691245,end:274691556,sha256:'4b0bd6b0e361d7ca08bce8d46cc617df7f935731d706c2e4ee128917cc5cb743'},
  {name:'outputSchema', start:263111312, end:263119537, sha256:'3bd4011accf3a704f5b4e724d33fd14db3f05bb26cd5cacfd0d8164a48e8ff8e'},
  {name:'response', start:274592937, end:274599187, sha256:'3128673ddff40ef84bfdce800e222f393c692563f2991c3776b6f84f45424fed'},
];
const sources = Object.fromEntries(ranges.map(range => {
  const bytes = binary.subarray(range.start,range.end);
  assert.equal(sha256(bytes),range.sha256);
  return [range.name,bytes.toString('utf8')];
}));
const context=vm.createContext({Buffer,_:JSON.stringify,d:()=>false,
  on:value=>value,_ne:()=>({}),q9r:()=>{},R3e:()=>null,T:Error});
const native=vm.runInContext(sources.expand+sources.provenance+sources.response
  +';({expand:s9r,provenance:QIr,response:r5})',context,{timeout:1000});
const payload={tool_input:{text:'a\ud800z',nil:null,record:{z:1,a:2},items:['zero','one'],
  '':{leaf:'empty-key'},empty:'',flag:false,n:0},mcp_server:{name:'audit',source:'user'}};
const templates=[
  '${tool_input.text}','before:${tool_input.text}:after','${tool_input.nil}',
  '${tool_input.absent}','${tool_input.record}','${tool_input.items}',
  '${tool_input.items.0}','${tool_input.items.1}','${tool_input.items.01}',
  '${tool_input.items.length}','${tool_input.items.length.length}',
  '${tool_input..leaf}','${tool_input.empty}','${tool_input.flag}','${tool_input.n}',
  '${tool_input.text.length}','${tool_input.text.0}','${tool_input.text',
  '${tool_input.text-}','${ tool_input.text}','${0}','${}',
  '${tool_input.中文}','${tool_input.\ud800}','${bad${tool_input.text}}',
  '${mcp_server.name}/${mcp_server.source}','${mcp_server..name}',
  '${tool_input.constructor}','${tool_input.toString}','${tool_input.__proto__}',
  '${tool_input.__proto__.constructor}','${tool_input.__proto__.__proto__}',
  '${tool_input.constructor.name}','${tool_input.items.map}',
  '${tool_input.items.__proto__}','${tool_input.items.__proto__.length}',

];
const templateCases=templates.map((template,index)=>({name:`template-${index}`,
  input_json:JSON.stringify({value:template,nested:[template,{again:template}]}),
  payload_json:JSON.stringify(payload),
  expanded_json:JSON.stringify(native.expand({value:template,nested:[template,{again:template}]},payload))}));
for (const input_json of [
  '{"__proto__":{"marker":"${mcp_server.name}"},"value":"kept"}',
  '{"nested":{"__proto__":"ignored","value":"${tool_input.text}"}}',
  '{"nested":[{"__proto__":null,"constructor":"${tool_input.n}"}]}',
]) {
  const template=JSON.parse(input_json);
  templateCases.push({name:'prototype-assignment-'+templateCases.length,input_json,
    payload_json:JSON.stringify(payload),expanded_json:JSON.stringify(native.expand(template,payload))});
}
const provenanceInputs=[
  {serverName:'sdk-lookalike',scope:'user'},
  {serverName:'sdk',serverType:'sdk',scope:'dynamic'},
  {serverName:'plugin',serverType:'sdk',pluginTelemetry:{}},
  {serverName:'explicit',source:'future-source',serverType:'sdk'},
  {serverName:'dynamic'},
  {serverName:'project',scope:'project'},
  {serverName:'managed',scope:'managed'},
  {serverName:'name\ud800',source:'user'},
];
const provenanceCases=provenanceInputs.map(input=>({input_json:JSON.stringify(input),
  expected_json:JSON.stringify(native.provenance(input))}));
const responseCases=[];
const select=['permissionBehavior','updatedInput','updatedToolOutput','updatedMCPToolOutput',
  'legacyMcpRewriteSuppressed','additionalContext','classifierContext','sessionTitle',
  'suppressOriginalPrompt','retry','elicitationResponse','elicitationResultResponse',
  'watchPaths','initialUserMessage','reloadSkills'];
function response(name,event,fields){
  const input={hookSpecificOutput:{hookEventName:event,...fields}};
  const result=native.response({json:input,command:'fixture',hookName:'fixture',
    toolUseID:'tool-1',hookEvent:event,expectedHookEvent:event,stdout:'',stderr:'',exitCode:0,durationMs:0});
  const output=Object.fromEntries(select.filter(key=>result[key]!==undefined).map(key=>[key,result[key]]));
  responseCases.push({name,event,input_json:JSON.stringify(input),expected_json:JSON.stringify(output)});
}
for(const value of [null,false,0,'',[],{},'rewritten'])
  response(`legacy-${JSON.stringify(value)}`,'PostToolUse',{updatedMCPToolOutput:value,classifierContext:'claim'});
response('direct-null','PostToolUse',{updatedToolOutput:null,classifierContext:'claim'});
response('direct-wins-pair','PostToolUse',{updatedToolOutput:{safe:true},updatedMCPToolOutput:false,classifierContext:'claim'});
response('session-title','SessionStart',{sessionTitle:'new title',watchPaths:['.env'],reloadSkills:true});
response('expansion-suppression','UserPromptExpansion',{additionalContext:'note',suppressOriginalPrompt:true});
response('permission-allow','PreToolUse',{permissionDecision:'allow',updatedInput:{command:'pwd'}});
response('permission-deny','PreToolUse',{permissionDecision:'deny',permissionDecisionReason:'policy'});
response('permission-defer','PreToolUse',{permissionDecision:'defer'});
response('elicitation-result','ElicitationResult',{action:'accept',content:{text:'a\ud800'}});
response('elicitation-cancel','Elicitation',{action:'cancel'});
response('ignored-output-fields','PostToolUse',{updatedInput:{x:true},action:'decline',watchPaths:['.env'],permissionDecisionReason:'ignored'});
// These stubs record the native schema builder's declared fields only; they
// do not pretend to execute Zod validation. This exposes union membership
// and unknown-field boundaries independently of the Rust parser.
function descriptor(kind,data={}) {
  const value={kind,...data};
  for(const method of ['describe','max','min','regex','refine']) value[method]=()=>value;
  value.optional=()=>descriptor(kind,{...data,optional:true});
  value.catch=()=>value;
  return value;
}
const shapeContext=vm.createContext({p:factory=>factory,
  u:fields=>descriptor('object',{fields}),
  C:value=>descriptor('literal',{value}),B:values=>descriptor('enum',{values}),
  o:()=>descriptor('string'),v:()=>descriptor('number'),I:()=>descriptor('boolean'),
  ie:()=>descriptor('unknown'),A:item=>descriptor('array',{item}),
  me:(_key,item)=>descriptor('record',{item}),Ue:variants=>descriptor('union',{variants}),
  aW:()=>descriptor('enum',{values:['allow','deny','ask','defer']}),
  as:()=>descriptor('permission-update'),Dv:()=>true});
const outputSchema=vm.runInContext(sources.outputSchema+';gY()',shapeContext,{timeout:1000});
const outputShapes=outputSchema.fields.hookSpecificOutput.variants.map(schema=>({
  event:schema.fields.hookEventName.value,
  fields:Object.entries(schema.fields).filter(([key])=>key!=='hookEventName')
    .map(([name,field])=>({name,type:field.kind,optional:field.optional===true,...field.values&&{values:field.values}})),
}));
const foldRewrite=vm.runInContext('function* fold(qr,_t,_s){'+sources.rewriteFold+'};fold',vm.createContext({t:()=>{}}),{timeout:1000});
const sequences=[
  ['same-hook-direct-and-legacy',[{updatedToolOutput:{direct:true},updatedMCPToolOutput:{legacy:true}}]],
  ['earlier-direct-later-legacy',[{updatedToolOutput:{first:true}},{updatedMCPToolOutput:{last:true}}]],
  ['earlier-legacy-later-direct',[{updatedMCPToolOutput:{first:true}},{updatedToolOutput:{last:true}}]],
  ['later-falsy-legacy',[{updatedToolOutput:{first:true}},{updatedMCPToolOutput:false}]],
  ['direct-null',[{updatedMCPToolOutput:{first:true}},{updatedToolOutput:null}]],
];
const rewriteSequences=sequences.map(([name,outputs])=>{
  const rewrites=outputs.flatMap(fields=>{
    const result=native.response({json:{hookSpecificOutput:{hookEventName:'PostToolUse',...fields}},
      command:'fixture',hookName:'fixture',toolUseID:'tool-1',hookEvent:'PostToolUse',expectedHookEvent:'PostToolUse',stdout:'',stderr:'',exitCode:0,durationMs:0});
    return Array.from(foldRewrite(result,'PostToolUse','fixture'),event=>
      Object.hasOwn(event,'updatedToolOutput')?{value:event.updatedToolOutput,legacy_mcp_only:false}
      :{value:event.updatedMCPToolOutput,legacy_mcp_only:true});
  });
  return {name,outputs_json:outputs.map(fields=>JSON.stringify({hookSpecificOutput:{hookEventName:'PostToolUse',...fields}})),
    expected_rewrites_json:JSON.stringify(rewrites)};
});
const publicAliases=vm.runInContext(sources.aliases+';Object.entries(l)',vm.createContext({}),{timeout:1000});
const legacyAliasGroups=[...new Set(publicAliases.map(([,canonical])=>canonical))].map(canonical=>({canonical,
  legacy:publicAliases.filter(([,value])=>value===canonical).map(([legacy])=>legacy)}));
const fixture={prototypeInterpreter:{nativeReplay:'Node.js vm; Bun builtin function formatting corroborated locally',candidateFallback:'Existing capability-free QuickJS sandbox; builtin native-function whitespace normalized to Bun text; 1s interpreter deadline, 8MiB heap'},version:'2.1.322',native_sha256:sha256(binary),ranges,templateCases,provenanceCases,responseCases,rewriteSequences,outputShapes,publicAliases,legacyAliasGroups,topDecisionValues:outputSchema.fields.decision.values,
  scope:'Controlled replay of native MCP hook interpolation, catalog provenance selection and parsed hook-result processing. Output union shapes are recorded by native schema-builder replay with descriptor-only stubs; this does not execute Zod validation. replay stubs only attachment/telemetry helpers. No transport execution or provider access.',
  unknowns:['Internal permissionDisplay cards, served/cloud hook policy and WorktreeCreate worktreePath routing are outside these fixtures.']};
const rendered=JSON.stringify(fixture,null,2)+'\n';
const fixturePath=new URL('../../crates/hooks/tests/fixtures/hooks_2_1_322.json',import.meta.url);
if(args.includes('--write'))writeFileSync(fixturePath,rendered);
else assert.equal(readFileSync(fixturePath,'utf8'),rendered,'hook oracle fixture drift');
console.log(`Claude Code 2.1.322 hooks: ${templateCases.length} interpolation, ${provenanceCases.length} provenance, ${responseCases.length} response cases match hash-pinned native helpers`);
