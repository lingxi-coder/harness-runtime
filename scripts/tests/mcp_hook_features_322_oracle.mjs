// Replay the exact feature-gated base-input helper from the shipped executable.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const binary = readFileSync(args.find(arg => arg !== '--write'));
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const range = {start:274587817, end:274588396, sha256:'d0f4a96b6a8f9a7967e01b12d339b530dbf5dda3fc1117d8f5ed1932984d9655'};
const source = binary.subarray(range.start, range.end);
assert.equal(sha256(source), range.sha256);
const state = {};
const context = vm.createContext({
  Ib:()=>undefined, tOr:()=>undefined, jh:()=>undefined,
  Hw:()=>false, $O:()=>undefined, q:()=>false, hm:()=>'', w$:()=>'prompt',
  zR:()=>state.enabled, uP:()=>state.directory,
  Qyr:/^[A-Za-z0-9][A-Za-z0-9_.-]{0,255}$/,
});
const native = vm.runInContext(source.toString()+';Ip', context, {timeout:1000});
const cases = [
  {name:'disabled', enabled:false, directory:'/session/scratchpad'},
  {name:'enabled-with-path', enabled:true, directory:'/session/scratchpad'},
  {name:'enabled-without-path', enabled:true, directory:null},
].map(input=>{
  Object.assign(state, input);
  const output = native({id:'fixture-session'}, '/workspace');
  // Session identity is host-owned. The selected base fields retain native order.
  const expected = Object.fromEntries(Object.entries(output).filter(([key])=>!['session_id','transcript_path'].includes(key)));
  return {...input, expected_json:JSON.stringify(expected)};
});
const fixture = {version:'2.1.322', nativeSha256:sha256(binary), range, cases};
const destination = new URL('../../crates/hooks/tests/fixtures/mcp_hook_features_2_1_322.json', import.meta.url);
if(args.includes('--write')) writeFileSync(destination, JSON.stringify(fixture,null,2)+'\n');
else assert.deepEqual(JSON.parse(readFileSync(destination)), fixture);
console.log(JSON.stringify({passed:true, cases:cases.length, scope:'native conditional scratchpad base-input helper'}));
