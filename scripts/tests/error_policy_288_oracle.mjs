#!/usr/bin/env node
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
import {nativeBetaWorld} from './common/anthropic_native_288.mjs';
const {binary,hash,ranges,take,context,messages,error}=nativeBetaWorld(process.argv[2]);
const identities=[
 ['dL=w(', ',irr=',180411500], ['nR=w(', ',Pbt=',180411200],
 ['G2e=w(', ',aY=',180411500], ['r$=w(', ',o$=',180412700],
 ['n$=w(', ',aW=',180411800], ['aW=w(', ',By=',180411800],
 ['By=w(', ',iy=',180411800], ['iy=w(', ',uL=',180411800],
].map(([first,last,offset])=>take(first,last,offset,'actual '+first.slice(0,-3)+' identity'));
const pieces=[
 take('function YFr(e)', 'function mie(',186226800,'thinking type copy'),
 take('function XFr(e)', 'function hie(',186227200,'structured format copy'),
 take('var fie=[', 'function KFr(',186225700,'native media copy lists'),
 take('function JFr(e)', 'var xF=',186227350,'native media field pattern'),
 take('function v4n(e)', 'function C4n(',186228600,'cache field copy and exclusions'),
 take('function x8(e)', 'function M8(',187036000,'media capabilities'),
 take('function kne(e)', 'function Sne(',187958500,'media guard'),
 take('function vne(e)', 'function lMe(',187967000,'afk beta guard'),
 take('Dvt=', ';var sZn=',187944400,'actual advisor type prefix'),
 take('function pMe(e)', 'function aMe(',187968600,'advisor entry guard'),
 take('function aMe(e)', 'function EEt(',187968900,'advisor type guard'),
 take('function mMe(e)', 'var TEt=',187969100,'cache keepalive and thinking resume'),
 take('function hMe(e)', 'function Tne(',187969400,'thinking binding guard'),
 take('function Cne(e)', 'function xne(',187972550,'mid-system position and tool guard composition'),
 take('function xne(e)', 'function vMe(',187973200,'tool changes'),
 take('function vMe(e)', 'function PEt(',187973900,'cache field and per-turn effort'),
 take('function EMe(e)', 'function Q0(',187974600,'timing guard'),
 take('function Q0(e)', 'function XO(',187974700,'effort and structured format'),
 take('function Pne(e)', 'function L2(',187976200,'thinking type and effort with thinking disabled'),
 take('function Gft(e,n)', 'function ',186030600,'native provider-model refusal dependency'),
 take('pF=(qn)=>', ',_Se=',189018000,'complete native pF composition'),
 take('P4e=(qn)=>{', ',I4e=',189000650,'actual unclaimed display gate'),
];
const extra=[take('function dMe(e)','var yeo=',187967800,'advisor result guard'),take('function ST(e)','function Obt(',180413000,'native full named-beta list projection')];
vm.runInContext(`${extra.join('\n')}var ${identities.join(';var ')};var ${pieces[8]};
${pieces.filter((_,i)=>i!==8&&i!==20&&i!==21).join('\n')}
var policyFlags={model_refused:false};
function pc(){return policyFlags.model_refused?'bedrock':'firstParty';}
function Hft(){return policyFlags.model_refused?'refused':'unknown';}
var ${pieces[20]};var ${pieces[21]};
var KM='idle',hostFailures=0,a={};
function IR(e){return GGt(e);}function He(){return 'firstParty';}
function ig(){return false;}function iM(){return false;}
function Ajt(){return hostFailures<2;}function Pjt(){hostFailures++;}
`,context);
const guardMessages=[...messages,
 'could not process image','could not process pdf','pdf pages exceed the maximum',
 'messages.0.content.1.image.source: invalid','messages[0].content[1].tool_result.content[2].pdf: invalid',
 'capability_rejected: image_block','capability_rejected: image_block_suffix',
 'Advisor tool result content could not be processed','found in advisor_tool_result blocks',
 'the advisor tool is not available','cannot be used as an advisor',
 'tools.5.model: invalid','tools.a.model: invalid',"Input tag 'advisor_20260301' found", "Input tag 'advisor_' found",
 'afk-mode-2026-01-31 anthropic-beta', 'cache-keepalive-2026-09-03 anthropic_beta',
 'thinking-resumption-2026-07-17 anthropic-beta',
 'thinking-binding-controls-2026-08-01 anthropic-beta',
 'thinking.adaptive.block_binding: Extra inputs are not permitted',
 'thinking.enabled.block_binding.prefix_mismatch_behavior: Extra inputs are not permitted',
 'thinking.type: enabled is not supported','thinking.type: adaptive is not supported',
 'thinking.type:::::::::adaptive is not supported','thinking.type:😀adaptive is not supported',
 'adaptive thinking is not supported','xadaptive thinking is not supported',
 'capability_rejected: thinking_type:enabled','capability_rejected: thinking_type:adaptive_extra',
 'role system must precede an assistant','use the top-level `system` parameter for the initial system prompt',
 "Input tag 'tool_addition'","Input tag 'tool_removal'","Input tag 'tool_definition'",
 'tool_addition after a paused assistant turn', 'tool_addition references an unknown tool',
 'tool_addition is PFC-only','tool_addition exceed definitions','tool_additions unsupported',
 'mid-conversation-tool-changes-2026-07-01 anthropic-beta','inline-tools-2026-09-15 anthropic-beta',
 'tool definitions are not available on this platform','tool.definition name is already used by a tool',
 'cannot yet be defined in a message','cache_control: unknown field','cache_control: additional properties',
 'cache_control: empty text block is not permitted','cache_control on system messages: not permitted',
 'system.0.cache_control: not permitted','cache_control tool_result: not permitted',
 'cache_control ttl: unknown field','cache_control TTL: unknown field',
 'capability_rejected: cache_control_field','capability_rejected: cache_control_fielding',
 'output_config.foo: unknown','output_config.format: Extra inputs are not permitted',
 'output_config.timing: unknown','per-turn-control-2026-07-01 anthropic-beta',
 'timing-2026-09-09 anthropic-beta','effort parameter is not supported',
 "effort 'high' is not supported when thinking is disabled",
 "messages.2: output_config.effort: effort 'high' is not supported when thinking is disabled",
 'capability_rejected: effort_unsupported','capability_rejected: structured_outputs_unsupported',
];
for(const separator of ['\u0085','\u00a0','\ufeff','\u2028','\r','😀']) {
 guardMessages.push(`cache_control role${separator}system: unknown field`, `adaptive${separator}thinking is not supported`, `messages.2:${separator}output_config.effort: effort 'high' is not supported when thinking is disabled`);
}
const policyCases=[];
for(const status of [0,200,400,401,422,500])for(const message of guardMessages)for(const body of [
 {type:'error',error:{type:'invalid_request_error',message}},{message},message,
])for(const model_refused of [false,true]){
 context.policyFlags={model_refused};const e=error(status,body);
 policyCases.push({status,body,model_refused,expected:context.pF(e)});
}
const displayCases=[];
for(const state of ['idle','retrying','spent'])for(const status of [400,422])for(const message of guardMessages){
 const body={type:'error',error:{type:'invalid_request_error',message}},e=error(status,body);
 context.policyFlags={model_refused:false};context.KM=state;context.kw=[context.cEe.header];context.Qe=[context.cEe];context.hostFailures=0;
 const retry=context.P4e(e)??null;
 displayCases.push({state,status,body,expected:{retry,state:context.KM,failures:context.hostFailures}});
}
const output={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,
 scope:'Actual complete pF boolean composition, native provider selection with host-supplied hosted refusal fact, and P4e with actual GGt. Hft availability classification, IR x4e fast-refusal history and full repair callback priority remain separate requirements.',
 policyCases,displayCases};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../deps/llm-client/tests/fixtures/error_policy_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(JSON.stringify({policyCases:policyCases.length,displayCases:displayCases.length}));
