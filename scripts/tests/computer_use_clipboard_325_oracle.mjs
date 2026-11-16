// Capability-free replay of exact reviewed 2.1.325 clipboard helpers.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const path=process.argv.slice(2).find(x=>!x.startsWith('--'));
if(!path)throw Error('Supply official 2.1.325 binary');
const hash=x=>createHash('sha256').update(x).digest('hex');
const binary=readFileSync(path);
assert.equal(hash(binary),'84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2');
const ranges=[['stash',302157775,302158121,'b5df1ed43192dfe2da8d243fb7464a8666f08fbdfbf81e0292fdc6fd4b4e6e2d'],['paste',302096930,302097294,'abe799875709dbffe2ab70589e89479ea7bf97091d8ea23676bf1fb9368766c8'],['type',302245875,302246977,'78c7fb745f8dc5de9f295e2e948d9ec4d816bf324a81cefcadf1eb4cea30d36e']];
const source=ranges.map(([name,a,b,h])=>{const bytes=binary.subarray(a,b);assert.equal(hash(bytes),h,name);return bytes.toString('utf8')}).join('\n');
function context(config){
 const ctx=vm.createContext({}, {codeGeneration:{strings:false,wasm:false}});
 vm.runInContext(`var config=${JSON.stringify(config)},events=[],clipboard='original',stash=undefined,reads=0;`,ctx);
 vm.runInContext(`
 async function W(){events.push(['read']);if(config.firstReadFails&&++reads===1)throw Error('read failed');return config.roundtripFails&&clipboard==='line1\\nline2'?'changed':clipboard;}
 async function K(text){events.push(['write',text]);if(config.restoreFails&&text==='original')throw Error('restore failed');clipboard=text;}
 async function X(ms){events.push(['sleep',ms]);}function t(){}
 var keyboard={async keys(keys){events.push(['keys',keys]);if(config.keyFails)throw Error('key failed');}};
 var env={executor:{capabilities:{platform:'darwin'},async readClipboard(){return W()},async writeClipboard(text){return K(text)},async type(text,opts){events.push(['type',text,opts])},async key(key){events.push(['key',key])}}};
 var state={grantFlags:{clipboardWrite:!!config.clipboardWrite},getClipboardStash(){return stash},onClipboardStashChanged(value){stash=value},isAborted(){return false}};
 function G(o,k){return o[k]}async function te(){return {block:null,approvedFrontmost:null}}async function ue(){return null}function P(text){return {content:[{type:'text',text}]}}function p(text){return P(text)}function Do(text){return [...text]}async function me(){}var et=8;
 `+source,ctx,{timeout:1000});return ctx;
}
const cases=[];
for(const config of [{},{firstReadFails:true},{roundtripFails:true},{keyFails:true},{restoreFails:true}]){
 const ctx=context(config);let error=null;try{await vm.runInContext("Ee(keyboard,'line1\\nline2')",ctx,{timeout:1000})}catch(e){error=e.message}
 cases.push({kind:'paste',config,error,...JSON.parse(vm.runInContext('JSON.stringify({events,clipboard})',ctx))});
}
for(const sequence of [[true,true,false],[true,false,true,false],[false],[true]]){
 const ctx=context({});for(const clear of sequence)await vm.runInContext(`Ue(env,state,${clear})`,ctx,{timeout:1000});
 cases.push({kind:'stash',sequence,...JSON.parse(vm.runInContext('JSON.stringify({events,clipboard,stash:stash??null})',ctx))});
}
for(const clipboardWrite of [false,true]){
 const ctx=context({clipboardWrite});const output=await vm.runInContext("zo(env,{text:'line1\\nline2'},state,{clipboardPasteMultiline:true})",ctx,{timeout:1000});
 cases.push({kind:'type',clipboardWrite,output,...JSON.parse(vm.runInContext('JSON.stringify({events})',ctx))});
}
const fixture={version:'2.1.325',binarySha256:hash(binary),sourceByteRanges:ranges.map(([name,start,end,sha256])=>({name,start,end,sha256})),scope:'Exact native CLI Ee and core Ue/zo replay with controlled clipboard, focus and keyboard effects. No OS clipboard, typing, capture or unknown host callback execution claim.',cases};
const output=JSON.stringify(fixture,null,2)+'\n',destination=new URL('../../crates/tools/computer-use/fixtures/clipboard-2.1.325.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),output);else writeFileSync(destination,output);
console.log(JSON.stringify({status:'passed',cases:cases.length,fixtureSha256:hash(output)}));
