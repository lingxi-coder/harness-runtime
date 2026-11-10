// Exact shipped SDK settings, trust/connector key and call-verdict gate replay.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import vm from 'node:vm';
const args=process.argv.slice(2),binaryPath=args.find(x=>!x.startsWith('--'));
const binary=readFileSync(binaryPath),sha=x=>createHash('sha256').update(x).digest('hex');
const expectedSha='a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
assert.equal(sha(binary),expectedSha);
const ranges=[];
function section(name,start,endToken){const end=binary.indexOf(endToken,start);assert(end>start);const bytes=binary.subarray(start,end);ranges.push({name,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
// Start exactly at G, retaining VKn, constants, lEt, dEt and Te unchanged.
const settingsStart=binary.indexOf('function G(e){return typeof e==="number"',261122100);
const settingsSource=section('sdk-settings',settingsStart,'var vk="mcp__";');
const normalizer=section('mcp-normalizer',260530248,'var e="claudeai_"');
const trusted=section('sdk-trusted-admission',267957847,'function QIr(');
const denyKey=section('sdk-connector-deny-key',267962360,'function b$n(');
const verdict=section('sdk-auto-mode-verdict',279489874,'function a(e){return e.scope===');
const marker=section('wire-input-text-marker',271232958,'function zqe(');
const context=vm.createContext({
 D:e=>e!==null&&typeof e==='object'&&!Array.isArray(e),
 EWt:e=>e==='sdk'||e==='sse-ide'||e==='ws-ide',
 nc:e=>e?.pluginSource!==undefined,
 d:(_name,_scope)=>false,
 s:128,uj:()=>false,S:()=>undefined,t:()=>{},c:()=>{},
 f:'claudecode/autoModeVerdict',R:'tengu_mighty_brook',k:()=>true,
 wxe:(id,ctx)=>ctx?.isMainSession===true&&ctx.agentId===id?undefined:id,
 ot:e=>e!==null&&typeof e==='object'&&!Array.isArray(e),
 Yjt:Symbol('wireToolInputs.textAsWritten'),_:JSON.stringify,g:()=>{},
});
vm.runInContext(settingsSource+normalizer+trusted+denyKey+marker+verdict,context);
const settingsInputs=[{},null,[],{connectorId:''},{connectorId:'  '},{connectorId:5},
 {connectorId:'crm.proxy',autoModeVerdictTools:['ping','', 'ping'],disableAutoBackground:true},
 {connectorId:'x'.repeat(129),autoModeVerdictTools:[]},
 {connectorId:'bad__key',disableAutoBackground:false},
 {connectorId:'one😀two',autoModeVerdictTools:['😀'.repeat(64)]},
 {connectorId:'one\ud800two',autoModeVerdictTools:['\ud800']},
 {autoModeVerdictTools:['x'.repeat(128)]},{autoModeVerdictTools:['x'.repeat(129)]},
 {autoModeVerdictTools:Array(64).fill('ping')},{autoModeVerdictTools:Array(65).fill('ping')},
 {autoModeVerdictTools:['ping',null]},{autoModeVerdictTools:'ping'},
 {disableAutoBackground:'true'},{disableAutoBackground:null},
];
// Build literal isolated UTF-16 inputs without replacing them with six characters.
settingsInputs[10].connectorId=JSON.parse('"one\\ud800two"');
settingsInputs[10].autoModeVerdictTools=[JSON.parse('"\\ud800"')];
const settingsCases=settingsInputs.map(input=>({inputSource:JSON.stringify(input),expectedSource:JSON.stringify(context.lEt(input).settings)}));
const trustInputs=[];
for(const type of ['sdk','http'])for(const scope of ['dynamic','project','user','agent'])for(const relayed of [false,true])for(const plugin of [false,true]){
 const config={type,name:'host-key',scope,relayed,connectorId:'crm.proxy',...(plugin?{pluginSource:{}}:{})};
 trustInputs.push({config,trusted:context.mbe(config),source:context.RF('host-key',config),denyKey:context.brn(config)??null});
}
for(const connectorId of ['x'.repeat(128),'x'.repeat(129),'bad__id','one😀two','claude.ai CRM Proxy']){
 const config={type:'sdk',name:'host-key',scope:'dynamic',connectorId};
 trustInputs.push({config,trusted:context.mbe(config),source:context.RF('host-key',config),denyKey:context.brn(config)??null});
}
function base(){return{
 config:{type:'sdk',name:'host-key',scope:'dynamic',autoModeVerdictTools:['ping']},
 rawTool:'ping',args:{q:1},toolUseId:'model-call',metaToolUseId:'model-call',
 enabled:true,autoModeClassified:true,hookCaller:false,clientRequestedToolUseId:null,
 parentIsVirtual:false,parentToolUses:[['model-call','mcp__host-key__ping']],mainConversation:true,
 pipelineAllows:true,pipelineClassifier:true,serverCleared:true,wireInputText:'{"q":1}',
 queryTools:[{modelName:'mcp__host-key__ping',serverName:'host-key',rawTool:'ping',source:'sdk'}],
};}
const changes=[{}, {enabled:false},{autoModeClassified:false},{config:{type:'http',autoModeVerdictTools:['ping']}},
 {config:{autoModeVerdictTools:[]}},{queryTools:[]},
 {queryTools:[...base().queryTools,{...base().queryTools[0],source:'plugin'}]},
 {toolUseId:null},{metaToolUseId:'forged'},{hookCaller:true},{clientRequestedToolUseId:'model-call'},
 {parentIsVirtual:true},{parentToolUses:[]},{parentToolUses:[['other','mcp__host-key__ping']]},
 {parentToolUses:[['model-call','different-tool']]},{mainConversation:false},{pipelineAllows:false},
 {pipelineClassifier:false},{serverCleared:false},{wireInputText:null},{wireInputText:'{ "q": 1 }'},
 {args:{q:2}},{args:{a:1,b:2},wireInputText:'{"b":2,"a":1}'},
 {queryTools:[{...base().queryTools[0],source:'project'}],config:{scope:'project'}},
 {queryTools:[{...base().queryTools[0],rawTool:'other'}]},
 {args:{'q':1},clientRequestedToolUseId:'another-call'},
];
const verdictCases=changes.map((patch,index)=>{
 const input={...base(),...patch,config:{...base().config,...patch.config}},meta={'claudecode/toolUseId':input.metaToolUseId,'claudecode/autoModeVerdict':'forged','other':'keep'};
 const parentMessage={isVirtual:input.parentIsVirtual,message:{content:input.parentToolUses.map(([id,name])=>({type:'tool_use',id,name}))}};
 if(input.wireInputText!==null){const marked={};Object.defineProperty(marked,context.Yjt,{value:input.wireInputText});parentMessage.wireToolInputs={[input.toolUseId]:marked};}
 const ctx={autoModeClassified:input.autoModeClassified,toolUseId:input.toolUseId??undefined,
  hookCaller:input.hookCaller?'hook':undefined,clientRequestedToolUseId:input.clientRequestedToolUseId??undefined,
  agentId:input.mainConversation?undefined:'child',options:{tools:input.queryTools.map(t=>({name:t.modelName,mcpInfo:{serverName:t.serverName,toolName:t.rawTool,source:t.source}}))},
  permissionPhase:{pipeline:{result:{behavior:input.pipelineAllows?'allow':'ask',decisionReason:{type:input.pipelineClassifier?'classifier':'mode',serverCleared:input.serverCleared}}}}};
 context.k=()=>input.enabled;
 context.Ccr(meta,{client:{name:'host-key',config:input.config},tool:{name:input.rawTool},args:input.args,parentMessage,context:ctx});
 return{name:`verdict-${index}`,input,expectedMeta:meta};
});
const fixture={version:'2.1.322',native_sha256:expectedSha,ranges,settingsCases,trustCases:trustInputs,verdictCases,
 scope:'Unchanged shipped function replay in Node VM. External environment feature toggles/owner registry and context construction are explicit boundary fixtures; positive server verdict uses host-supplied facts and a hidden marker, never a local classifier or real account.'};
const output=args.find(x=>x.startsWith('--source-output='))?.slice(16);
if(output){mkdirSync(output,{recursive:true});for(const[name,source]of Object.entries({settings:settingsSource,normalizer,trusted,denyKey,marker,verdict}))writeFileSync(`${output}/${name}.js`,source);writeFileSync(`${output}/ranges.json`,JSON.stringify({native_sha256:expectedSha,ranges},null,2)+'\n');}
const target=new URL('../../crates/mcp/tests/fixtures/mcp_sdk_policy_2_1_322.json',import.meta.url),rendered=JSON.stringify(fixture,null,2)+'\n';
if(args.includes('--write'))writeFileSync(target,rendered);else assert.equal(readFileSync(target,'utf8'),rendered);
console.log(`PASS SDK policy native replay: ${settingsCases.length} settings / ${trustInputs.length} trust+alias / ${verdictCases.length} verdict gates`);
