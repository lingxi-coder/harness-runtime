#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const start=binary.indexOf('function _(e,n,r){',178066645),end=binary.indexOf('function fD(e)',start);
assert(start>=178066645&&end>start);const bytes=binary.subarray(start,end);
const scope=vm.createContext({});vm.runInContext(`const ng=()=>({[Symbol.dispose]() {}}),Se=x=>x;${bytes.toString('utf8')}globalThis.stringify=_;`,scope);
const cases=[],seen=new Set(),buf=Buffer.alloc(8);
function append(value){if(!Number.isFinite(value))return;buf.writeDoubleBE(value);const bits=buf.toString('hex');if(seen.has(bits))return;seen.add(bits);
 const body={number:value,before:true,'2':value,'0':value,'01':value,nested:[{number:value},-0]};
 cases.push({bits,expected_string:String(value),expected_json:scope.stringify(value),expected_body_json:scope.stringify(body)});
}
for(const value of [0,-0,1,-1,0.1,0.2,1.2345678901234567,1.0000000000000002,1e-6,9.999999999999997e-7,1e-7,1e20,1e21,1e23,1000000000000000128,9007199254740992,9007199254740994,Number.MIN_VALUE,Number.MAX_VALUE])append(value);
let state=0x8b5ad4cef9287123n;for(let i=0;i<4096;i++){state^=state<<13n;state^=state>>7n;state^=state<<17n;state=BigInt.asUintN(64,state);buf.writeBigUInt64BE(state);append(buf.readDoubleBE());}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:[{label:'instrumented native JSON.stringify wrapper',start,end,sha256:sha(bytes)}],scope:'Actual native JSON.stringify wrapper with measurement supplied. IEEE754 bit fixtures avoid a lossy decimal fixture parse; Node ECMAScript builtin, not embedded Bun engine or complete SDK request composition.',cases};
const json=JSON.stringify(output,null,2)+'\n';
for(const path of ['../../deps/llm-client/tests/fixtures/json_numbers_2_1_287.json','../../crates/llm-runtime/tests/fixtures/json_numbers_2_1_287.json']){const destination=new URL(path,import.meta.url);if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);}
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native IEEE754 JSON cases`);
