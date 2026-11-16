// Exact, reviewed TCC adapter and request_access replay from official 2.1.325.
// Native TCC calls are replaced with boolean fixtures; no Claude bootstrap,
// credentials, network, target applications or native effects are executed.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';
const path = process.argv.slice(2).find(arg => !arg.startsWith('--'));
if (!path) throw new Error('Supply the official Claude Code 2.1.325 darwin-arm64 binary');
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const binary = readFileSync(path);
const nativeSha256 = '84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2';
assert.equal(digest(binary), nativeSha256);
const ranges = [
  {
    "name": "p",
    "start": 302154067,
    "end": 302154167,
    "sha256": "4136c18c67daaa1aa4c308a09c7d3c01e6fecaffcea395698c590a0e32bced04"
  },
  {
    "name": "G",
    "start": 302155308,
    "end": 302155408,
    "sha256": "88edea3e13af43e48dfd8db53054a606980bcfef9a77abddfe43c87d5b319d9d"
  },
  {
    "name": "Fo",
    "start": 302218071,
    "end": 302222642,
    "sha256": "49f87b479dfae797b0781b40ba71c4db1c0dd94b2631378f64f3ff3c169252e3"
  },
  {
    "name": "ensureOsPermissions",
    "start": 302276857,
    "end": 302277010,
    "sha256": "9f025521fed5d876e796bb23b5dc7a5a806077a2198b85b0e39617ee56f3b65a"
  }
];
const source = {};
for (const range of ranges) {
  const bytes = binary.subarray(range.start, range.end);
  assert.equal(digest(bytes), range.sha256, range.name);
  source[range.name] = bytes.toString('utf8');
}
const context = vm.createContext({}, { codeGeneration: { strings: false, wasm: false } });
vm.runInContext([source.p, source.G, source.Fo,
  `globalThis.ensure = (${source.ensureOsPermissions});`,
  `var state, trace, seen;
   function Ne() { return 'controlled-request'; }
   function F8() { return {tcc:{
     checkAccessibility() { trace.push('checkAccessibility'); return state[0]; },
     checkScreenRecording() { trace.push('checkScreenRecording'); return state[1]; },
   }}; }`,
].join('\n'), context, { timeout: 1000 });
const plain = value => JSON.parse(JSON.stringify(value));
const states = [[false,false],[false,true],[true,false],[true,true]];
const fixture = {version:'2.1.325',nativeSha256,sourceByteRanges:ranges,
  scope:'Exact OS-grant adapter and TCC-only request_access branches with deterministic native boolean seams. Does not execute native TCC, display/background actions, permissions UI, or full request_access app-selection logic.',
  brandingSubstitution:{from:'Claude desktop app',to:'LingXi desktop app'},
  permissionStates:[],setupTransitions:[]};
for (const state of states) {
  vm.runInContext(`state=${JSON.stringify(state)};trace=[];`,context);
  const expected=plain(await vm.runInContext('ensure()',context,{timeout:1000}));
  fixture.permissionStates.push({state,expected,trace:plain(vm.runInContext('trace',context))});
}
for (const before of states.filter(s=>!s[0]||!s[1])) for (const after of states) {
  const input={before,after};
  vm.runInContext(`state=${JSON.stringify(after)};trace=[];seen=[];`,context);
  const expected=plain(await vm.runInContext(`Fo({
    executor:{capabilities:{screenshotFiltering:'native'}},ensureOsPermissions:ensure
  }, {reason:'set up computer access'}, {
    onPermissionRequest:async request=>{trace.push('permissionPanel');seen.push(request);}
  }, ${JSON.stringify({accessibility:before[0],screenRecording:before[1]})})`,context,{timeout:1000}));
  const branded = expected.content[0].text.replaceAll('Claude desktop app','LingXi desktop app');
  fixture.setupTransitions.push({input,expected,expectedModelTextJson:JSON.stringify(branded),trace:plain(vm.runInContext('trace',context)),requests:plain(vm.runInContext('seen',context))});
}
const output=new URL('../../crates/tools/computer-use/fixtures/tcc-2.1.325.json',import.meta.url);
const rendered=JSON.stringify(fixture,null,2)+'\n';
if(process.argv.includes('--write'))writeFileSync(output,rendered);
else assert.equal(readFileSync(output,'utf8'),rendered,'TCC 2.1.325 oracle drift');
console.log(`TCC 2.1.325: ${fixture.permissionStates.length} grant states and ${fixture.setupTransitions.length} setup transitions match ${nativeSha256}`);
