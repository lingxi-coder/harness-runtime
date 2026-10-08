import fs from 'node:fs';
import vm from 'node:vm';
import path from 'node:path';
import { createHash } from 'node:crypto';
const source = fs.readFileSync(path.join(process.argv[2], 'src_183727495.js'),'utf8');
const utilities = fs.readFileSync(path.join(process.argv[2], 'src_176129258.js'),'utf8');
for (const [text, expected] of [
  [source, '5f2e79dc208fa1829ae1b7e8862ea500c7b40b8a9bb26b51b9706852f7f79f92'],
  [utilities, 'fe0741faf248230abb7edcea60cb5ebea594b5ff17fa0f06fdcb4717ea0f990a'],
]) {
  if (createHash('sha256').update(text).digest('hex') !== expected) {
    throw Error('Tool-result oracle source differs from the pinned 2.1.286 chunk');
  }
}
const context = vm.createContext({
  Buffer, S: JSON.stringify, fM: 50000,
  Mu: value => value !== null && typeof value === 'object' && !Array.isArray(value),
  I: (count,word) => count === 1 ? word : word+'s',
});
for(const name of ['f','re','Q2e']) {
  const at=utilities.indexOf(`function ${name}(`);
  const end=utilities.indexOf('function ',at+9);
  if(at<0||end<0) throw Error(`missing upstream ${name}`);
  vm.runInContext(utilities.slice(at,end),context);
}
const start=source.indexOf('var Ngn='), end=source.indexOf('function jw(',start);
if(start<0||end<0) throw Error('missing upstream normalization anchors');
vm.runInContext(source.slice(start,end),context);
const inputs=[{name:'object',value:{ok:true,count:2}},{name:'boolean',value:false},{name:'number',value:42},
 {name:'js_number_and_keys',value:JSON.parse('{"z":1.0,"10":10,"2":2,"small":1e-7,"zero":-0.0}')},
 {name:'string_unchanged',value:'already text'},
 {name:'surrogate_safe_limit',value:{value:'x'.repeat(49990)+'😀zz'}}];
const cases=inputs.map(({name,value})=>({name,input:value,expected:context.b5e([{type:'user',message:{content:[{type:'tool_result',content:value}]}}])[0].message.content[0].content}));
fs.writeFileSync(new URL('cases.json',import.meta.url),JSON.stringify(cases,null,2)+'\n');
