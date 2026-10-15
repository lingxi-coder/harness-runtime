#!/usr/bin/env node
// Run exact current native nested-memory and compact file-restore helpers.
// Usage: node scripts/tests/nested_compact_restore_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { open } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import vm from 'node:vm';

assert(process.argv[2], 'Pass the current native binary');
const binary = readFileSync(process.argv[2]);
const nativeSha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const pins = {
  "native Sv.create checkPermissions/validateInput/call methods": "548d07173c551631ed7b211653d347dd3fc108ae64355c358c443be1f39689e1",
  "function g_n(": "037f143bdc3c2d4c277319dc1508dc28cded8df1da804381afb83308b0815d68",
  "function Ww(": "7b76ff3a2e143625ab33e33e55e8cffec4b03cc9f60337f3a5599c85872adf60",
  "function pZ(": "1709891fe4c79d473a6b0a0832b8c25110c12dd5b0f0e8fe52c8a26d1ffc8770",
  "var C3=": "8f488640a6ebd0cc63eb390ebea11147067412e127c5dab157e59e1275764c85",
  "var nor=": "dc6f79788cb08b1e3a7a1e6eb3f9b875d82bd400ce93743e13ba161cf1a85078",
  "function m9e(": "99fff3a117fb0f0dbbce49ea9f700e901f3e81dc1a338ddc93b8a9c6f1768076",
  "function csn(": "377955dae65138c818544bfcf0e6de75df02d75ceea0309fcc145fb549dcf706",
  "function hdt(": "3909a693ced6889d43b51a0b4e8a890474d41a11f056c4d71c1d0fa0a7948318",
  "function ai(": "5b2477082f0e7bf0d8eec9882d623deac5300bd4c0e4dc32027c384b1f8031a0",
  "async function tzn(": "9fb38341dfe9bf7b2bd2a77a6aa451594cf31cd1e290352d458f2609566e4ee9",
  "function Trr(": "1651bd446b2f345d81b18cd1b90ca28037da651f84b10810a1f467eab2d1d42d",
  "function a1e(": "ddf8877937039eb89356f1d3873557b22d9c235e6cd4d520b3f04eebe85364ea",
  "async function m7t(": "b27514ac09ca2e0127a16e804714cf1e22bdbac0748f09c650ff1e4dc0ab73c4",
  "async function O7t(": "31b0acbf6bd8091d170d3588b8fb46f9b5152b8c5fc42252f6fbad367417ab0f",
  "async function Iie(": "e991d46ecf455ed61ca6228cc6300acabb9f9648bbd5f52fcf271c68f8b3fd05",
  "async function Evo(": "ee97cd2febe92b846f5393c2e52256266c1c4476a9eb31b96827fc959bac2130",
  "function Mvo(": "75be7f01fb54a21328cbc69f32c588fd6f6fc4bc4d4b136f0517df0d8376c571",
  "function Ovo(": "0f28468a65933bd22046055484ff215d6e5dd9352ab63764cce33b89dc246a60",
  "function Dvo(": "1423d00a352cbf2bc25061e105ea670f66f10e25d2fe9721be242c40d499ec32",
  "async function JOe(": "1202c4190a66914dd5ea0f1f634a8e4bad64c0d3d0924eacf8a5037162169163",
  "async function uLt(": "a3bc0976b85f891aa497430eda0964f492348b8d17e0715c04b19cc33e8dee8d",
  "function ln(": "deb14c47567453008fec0b6cf75f119ed8e5296d7433a2b367809722fc098602",
  "function LCe(": "8f133fd54b099bad4d65bb420710ac701b5ec5d9e6a9245460f145a7b2e00063",
  "async function AKn(": "4c41e6f5fafa81d3c3799d46b1b1fea9e70c76877c4ccaa1e550cf0a8f9c6b30",
  "async function qgt(": "9a9c8d45bb8fc319bcbc2b76e7e3085664d47f8b216ed8686bdba4924d2c9ea9",
  "async function IKn(": "278e91116c205aabf618e97fc06c638879a90c7aa8d0481962bc4563ef9c9740",
  "async function Ngt(": "234d86d775de3567c98fefad32e9c6bba58448bc5f98e6cca993f288628c7ea6",
  "native pure schema 178308215": "7937729d686f4b04d732397a99a2b5bc051de27751c50ca2c80a23ec84890644",
  "native pure schema 178319858": "bf0b9819eea4ab1b4e8a70ffb924d5f5a1a62c148ea2ebf839429258515b91aa",
  "native pure schema 178237228": "91b433ccc5add9cada15ca727fd71e45dc2b3042168625a06d879b12cab45945"
};
const sourceByteRanges = [];
function chunk(offset) {
  const marker = '// @bun @bytecode\n';
  assert.equal(binary.subarray(offset, offset + marker.length).toString(), marker);
  const end = binary.indexOf(marker, offset + marker.length);
  const bytes = binary.subarray(offset, end < 0 ? binary.length : end);
  const nul = bytes.indexOf(0, Math.max(bytes.lastIndexOf('export{'), 0));
  return bytes.subarray(0, nul < 0 ? bytes.length : nul);
}
function record(bytes, start, label) {
  const sha256 = createHash('sha256').update(bytes).digest('hex');
  assert.equal(sha256, pins[label], `Changed actual native span: ${label}`);
  sourceByteRanges.push({label, start, end:start + bytes.length, sha256});
  return bytes.toString('utf8');
}
function take(offset, marker, endMarker, label=marker, searchFrom=0) {
  const bytes = chunk(offset);
  const start = bytes.indexOf(marker, searchFrom);
  assert(start >= 0, marker);
  let end;
  if (endMarker) end = bytes.indexOf(endMarker, start + marker.length);
  else {
    end = bytes.indexOf('function ', start + marker.length);
    if (bytes.subarray(end - 6, end).toString() === 'async ') end -= 6;
  }
  assert(end > start, `${marker} end`);
  return record(bytes.subarray(start,end), offset + start, label);
}
function fn(marker, endMarker) { return take(185762130, marker, endMarker); }
function pureModule(offset, globals={}) {
  const source = record(chunk(offset), offset, `native pure schema ${offset}`)
    .replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, 'return {$1};');
  return vm.runInNewContext(`(()=>{${source}})()`,globals,{timeout:2000});
}
const schemas = pureModule(178308215);
Object.assign(schemas, pureModule(178319858, schemas), pureModule(178237228));
const svStart = chunk(185762130).indexOf('var Sv=');
assert(svStart >= 0);
const createRead = take(185762130, 'create(e){return{async checkPermissions',
  ',mapToolResultToToolResultBlockParam',
  'native Sv.create checkPermissions/validateInput/call methods',svStart);
const source = [
  take(181978701, 'function g_n('),
  take(181978701, 'function Ww(', 'function o6o('),
  take(180615120, 'function pZ('),
  fn('var C3=', 'function iAo('),
  fn('var nor=', 'var oor='),
  fn('function m9e('), fn('function csn('), fn('function hdt('), fn('function ai('),
  fn('async function tzn(', 'import{isAbsolute as uKn'),
  fn('function Trr('), fn('function a1e('), fn('async function m7t('), fn('async function O7t('),
  fn('async function Iie('), fn('async function Evo('), fn('function Mvo('),
  fn('function Ovo('), fn('function Dvo('), fn('async function JOe('),
  fn('async function uLt(', 'function ln('), fn('function ln('),
  fn('function LCe(', 'var Sv='), fn('async function AKn(', 'var Jgt='),
  fn('async function qgt('), fn('async function IKn(', 'async function gOr('),
  fn('async function Ngt('),
].join('\n');
// Every virtual path is mapped into this run's own temporary directory.
const ioRoot = mkdtempSync(path.join(tmpdir(), 'nested-compact-restore-287-'));
const full = name => path.join(ioRoot, name.replace(/^\//,''));
const normalize = value => JSON.parse(JSON.stringify(value));
class FixtureDate extends Date {
  constructor(...args) { super(...(args.length ? args : ['2026-10-01T00:00:00.000Z'])); }
  static now(){return 1000}
}
function setup() {
  const telemetry = [], hooks = [], errors = [], reads = [];
  let uuid = 0;
  const stash = {holds:()=>true,takeIdentity:()=>undefined};
  const ctx = {session:{host:{},writePermissionStash:stash},options:{tools:[],mcpClients:[],mainLoopModel:'fixture',settingsHooksOff:true},
    readFileState:new Map, messages:[], signal:new AbortController().signal,
    mainLoopModel:()=> 'fixture', permissions:()=>({mode:'default'}),
    nestedMemoryAttachmentTriggers:[], dynamicSkillDirTriggers:[], taskRegistry:new Map};
  const unrelated = {
    qo:()=>false, xvo:async()=>[], Avo:()=>[], Pvo:()=>[], Cvo:async()=>null, Rvo:()=>null,
    J5e:()=>undefined,l0o:()=>[],dLt:async()=>[],Jzn:async()=>[],BHr:()=>[],jHr:()=>[],Ddt:()=>[],
    i0o:()=>[],a0o:()=>[],l9e:()=>[],nSt:()=>[],Vg:()=>false,Lm:()=>null,H5:async()=>[],ZOe:()=>null,
    $Hr:()=>undefined,
  };
  const globals = {...schemas,...unrelated,Map,Set,Buffer,Date:FixtureDate,
    Bi:row=>row?.type==='system'&&row.subtype==='compact_boundary',
    Aat:async()=>[],F:()=>false,yGn:()=>true,Eb:async()=>77,
    pv:path.relative,oe:()=>'/repo',fv:(name,dir)=>name===dir||name.startsWith(dir+'/'),
    UXt:path.join,HXt:path.relative,
    Wan:(_session,filename,type,reason,extra)=>hooks.push(normalize({filename,type,reason,extra})),
    d:error=>errors.push(String(error)), hg:()=>`nested-${++uuid}`,
    Xe:name=>path.resolve(name),We:path.join,we:()=>'/home/.claude',Ee:()=>'/repo',gT:()=>'/managed',Goe:()=>'/home/.claude/projects/repo/memory/MEMORY.md',
    $w:()=>'/home/.claude/plans/fixture.md',x$o:()=>false,
    HOe:5,jOe:5000,yvo:50000,Nc:text=>Math.ceil(text.length/4),_:JSON.stringify,
    OMt:()=>({}),Dfe:()=>`attachment-${++uuid}`,
    pe:()=>({mode:'default'}),U6:()=>false,
    i:(name,args)=>telemetry.push({name,args}),
    dt:'Read',kk:()=>false,uk:()=>undefined,Aa:()=>null,Ln:()=>false,
    vYe:()=>false,DJe:()=>false,wKn:()=>false,jh:path,
    NCe:()=>undefined,oWe:(_ctx,name)=>[name],O:()=> 'linux',
    $Q:()=>({maxTokens:5000,maxSizeBytes:262144}),UU:()=>false,k:(_name,fallback)=>fallback,
    a:{CLAUDE_CODE_SIMPLE:true},X:()=>({pendingHandoverReads:new Map}),
    ql:()=>({}),W6t:new Set,nht:()=>false,
    lUt:()=>false,Wgn:async(name)=>{const handle=await open(full(name),'r');return {ioPath:full(name),canonicalPath:name,handle,close:()=>handle.close()};},
    Lre:error=>{throw error},
    Ake:async(name,offset,limit,_max,_signal,{handle})=>{
      reads.push(name);
      const content=await handle.readFile({encoding:'utf8'});
      assert.equal(offset,0);assert.equal(limit,undefined);
      return {content,lineCount:content.split('\n').length,totalLines:content.split('\n').length,
        totalBytes:Buffer.byteLength(content),readBytes:Buffer.byteLength(content),mtimeMs:77};
    },
    T9e:()=>null,CXe:text=>Math.ceil(text.length/4),JVr:4,
    AS:()=>false,Jgt:new WeakMap,sht:new WeakMap,CS:()=>{},TKn:()=>undefined,_U:()=>undefined,S:value=>value,
    IPe:class extends Error{},Tut:class extends Error{},ZLt:class extends Error{},Ye:class extends Error{},
  };
  const sandbox=vm.createContext(globals);
  const native=vm.runInContext(`(()=>{${source};return {m7t,O7t,g_n,Iie,Evo,uLt,pZ,Mvo,makeRead(e){return ({${createRead}}).create(e)}}})()`,sandbox,{timeout:2000});
  sandbox.Sv=native.makeRead(ctx);
  return {native,ctx,telemetry,hooks,errors,reads};
}

const cases=[];
try {
for(const retained of [false,true]){
  const {native,ctx,telemetry,hooks,errors,reads}=setup();
  const file={path:'/repo/pkg/LINGXI.md',type:'Project',content:'Folder guidance',contentDiffersFromDisk:false};
  mkdirSync(path.dirname(full(file.path)),{recursive:true});writeFileSync(full(file.path),file.content);
  const initial=await native.m7t([file],ctx,'/repo/pkg/handler.rs',{nested:new Map,announced:undefined});
  assert.equal(initial.length,1);
  const snapshot=native.g_n(ctx.readFileState);
  assert.equal(snapshot[file.path].seededFromContext,true);assert.equal(snapshot[file.path].keepContent,true);
  ctx.readFileState.clear();
  const history=retained?[{type:'attachment',uuid:'initial',attachment:initial[0]}]:[];
  const restored=await native.Iie(snapshot,ctx,history,'compact_full',history,'fixture');
  assert.equal(restored.attachments.length,1,JSON.stringify({restored,errors,telemetry}));
  assert.equal(restored.attachments[0].attachment.type,'file');
  assert.equal(restored.attachments[0].attachment.filename,file.path);
  assert.equal(ctx.readFileState.get(file.path).content,file.content);
  assert.equal(ctx.readFileState.get(file.path).offset,1);
  const baseline=await native.O7t(ctx,history);
  const subsequent=await native.m7t([file],ctx,'/repo/pkg/handler.rs',baseline);
  assert.equal(subsequent.length,0);
  assert.deepEqual(errors,[]);
  cases.push(normalize({name:retained?'retained-nested-seed-restores-file':'discarded-nested-seed-restores-file',
    input:{file,history,trigger:'/repo/pkg/handler.rs'},
    expected:{initial,snapshot,restored,afterReadState:[...ctx.readFileState],subsequent,hooks,reads,telemetry,errors}}));
}
{
  // This case supplies a fresh empty cache explicitly. It does not implement
  // or claim coverage of the native resume initializer's cache restoration.
  const {native,ctx,hooks,errors}=setup();
  const file=cases[0].input.file;
  const restoredFile=cases[0].expected.restored.attachments[0];
  const history=[{type:'system',subtype:'compact_boundary',uuid:'fixture-boundary'},restoredFile];
  assert.equal(native.Mvo([restoredFile]).size,0,'Preserved raw file attachments are not preserved Read tool calls');
  const baseline=await native.O7t(ctx,history);
  assert.equal(baseline.nested.size,0);
  const attachments=await native.m7t([file],ctx,'/repo/pkg/handler.rs',baseline);
  assert.equal(attachments.length,1);
  assert.deepEqual(errors,[]);
  cases.push(normalize({name:'controlled-fresh-resume-file-history-emits-nested',
    input:{file,history,trigger:'/repo/pkg/handler.rs',readState:[],controlledFreshResume:true},
    expected:{announced:baseline.announced,nested:[...baseline.nested],attachments,
      readState:[...ctx.readFileState],hooks,errors}}));
}
for(const tier of ['Managed','User','Project','Local','AutoMem']){
  const {native,ctx,telemetry,hooks,errors,reads}=setup();
  const filename=native.pZ(tier);
  const file={path:filename,type:tier,content:`${tier} guide`,contentDiffersFromDisk:false};
  const initial=await native.m7t([file],ctx,'/repo/pkg/handler.rs',{nested:new Map});
  const snapshot=native.g_n(ctx.readFileState);ctx.readFileState.clear();
  const restored=await native.Iie(snapshot,ctx,[],'compact_full',[],'fixture');
  assert.equal(restored.attachments.length,0,JSON.stringify({tier,restored,errors}));
  assert.equal(reads.length,0);assert.equal(ctx.readFileState.size,0);assert.deepEqual(errors,[]);
  cases.push(normalize({name:`eager-${tier}-entrypoint-excluded`,
    input:{file,history:[],trigger:'/repo/pkg/handler.rs'},
    expected:{filename,initial,snapshot,restored,afterReadState:[...ctx.readFileState],hooks,reads,telemetry,errors}}));
}
} finally {
  rmSync(ioRoot, {recursive:true,force:true});
}
const output={version:'2.1.287',native_sha256:nativeSha256,sourceByteRanges,
  scope:'Actual unchanged m7t/O7t/l1e/g_n/Iie/Evo/uLt/Sv.create validateInput+call/LCe/AKn/qgt/IKn/Ngt/Mvo/Ovo/Dvo/JOe/pZ; whole native plain schema modules are executed. Selected text files are read from a private real temporary directory through controlled Node file-handle/stat/line-reader adapters. Small ordinary text fixtures stay below the controlled token budget. Map represents the readFileState host, excluding native LRU internals. Permission, absent unrelated Iie restore capabilities, artifact-path predicate (ordinary paths only), telemetry/hooks, UUID and clock are controlled. Source helper bodies are unchanged and individually hash-checked against the current full-binary SHA. One case supplies controlled fresh resume ReadState with post-boundary file history; it does not execute or validate the native cold/hot resume initializer. This is a Node helper oracle, not native CLI or Bun acceptance. The folder filename uses the permitted LingXi branding difference; five eager entrypoints retain native pZ bytes.',
  cases};
const fixture=fileURLToPath(new URL('../../crates/compaction/tests/fixtures/nested_compact_restore_2_1_287.json',import.meta.url));
const serialized=JSON.stringify(output,null,2)+'\n';
if(process.argv.includes('--check')) {
  assert.equal(readFileSync(fixture,'utf8'),serialized,'Current native nested compact restore fixture differs');
  process.stdout.write(`checked native 2.1.287 nested compact restore oracle: ${cases.length} cases\n`);
} else {
  mkdirSync(path.dirname(fixture),{recursive:true});
  writeFileSync(fixture,serialized);
  process.stdout.write(`wrote native 2.1.287 nested compact restore oracle: ${cases.length} cases\n`);
}
