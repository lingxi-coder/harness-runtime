// Run the actual native Zod implementation, English locale, tool schemas and
// diagnostic formatter extracted from the official SHA-pinned 2.1.270 binary.
// Usage: node monitor-schema-2.1.270.mjs /tmp/lingxi-loop-oracle-2.1.270
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import crypto from 'node:crypto';
import {pathToFileURL} from 'node:url';
const root=process.argv[2];
const sha=s=>crypto.createHash('sha256').update(s).digest('hex');
const binarySha256=sha(fs.readFileSync(path.join(root,'package/claude')));
if(binarySha256!=='a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807')throw Error('Wrong oracle binary');
const sources={};
const read=name=>{let s=fs.readFileSync(path.join(root,'chunks',name),'utf8');sources[name]=sha(s);return s};
read('src_165047929.js');
const z=await import(pathToFileURL(path.join(root,'chunks/src_165047929.js')).href);
const ctx=vm.createContext({...z,f:fn=>fn});
vm.runInContext(read('src_198617341.js').replace(/import\{[^}]*\}from"[^"]*";/g,''),ctx);
const main=read('src_169588164.js');
function between(s,start,end){let a=s.indexOf(start);if(a<0)throw Error(start);let b=s.indexOf(end,a);if(b<0)throw Error(end);return s.slice(a,b)}
{ const start=main.indexOf('function ewn('); vm.runInContext(main.slice(start,main.indexOf('function ',start+10)),ctx); }
vm.runInContext(between(main,'function Yge(','function i6('),ctx);

ctx.URL=URL;ctx.Fje=300000;ctx.rnt=3600000;ctx.Vye=()=>1800000;ctx.bounded=false;ctx.Az=()=>ctx.bounded;ctx.a8=()=>false;
// Imported sumBy utility used by the exact one-source refinement.
ctx.j=(values,iterate)=>values.reduce((total,value)=>total+Number(iterate(value)),0);
ctx.MB=e=>Array.isArray(e.allowed_domains)&&e.allowed_domains.length?{allow:e.allowed_domains}:undefined;
const controls=read('src_167716440.js');
ctx.AD=vm.runInContext('(function(){'+between(controls,'function Z(','function Q(')+between(controls,'function AD(','function Es(')+';return AD})()',ctx);
const monitor=read('src_177711820.js');
vm.runInContext(between(monitor,'var de=','var Ie='),ctx);
const exporter=read('src_165140675.js');
vm.runInContext(between(exporter,'var qg=new Set(','function Zn('),ctx);
const cases=[];const schemas={};
const values=[null,false,true,0,1.5,'',[],{},'true','false','120',999,1000,1000.5,3600000,3600001];
for (const bounded of [false,true]) {
 ctx.bounded=bounded;const mode=bounded?'bounded':'legacy';const schema=vm.runInContext('Ce()',ctx);schemas[mode]=vm.runInContext('wde(Ce(),{unrepresentable:"throw"})',ctx);
 const base={description:'d',command:'echo ready'};
 const inputs=[null,false,0,'',[],{},base,{...base,extra:1},{description:'d'},{...base,ws:{url:'wss://example.com'}},JSON.parse('{"description":"d","command":"echo","__proto__":true}')];
 for (const field of ['description','command','ws','timeout_ms','persistent']) for (const value of values) inputs.push({...base,[field]:value});
 for (const command of ['', ' ', 'echo\nready','echo\rready','echo\tready','echo\u0000ready','echo\u007fready','echo\u0085ready','echo\u202eready']) inputs.push({...base,command});
 for (const url of ['wss://example.com','WS://EXAMPLE.COM','ws:example.com','ws://user@example.com','wss://example.com/a b','wss://example.com/\t','wss://example.com/\n','wss://example.com/\r','https://example.com','wss://例子.com','wss://[::1]','wss://example.com:99999','wss://%65xample.com','wss://@example.com']) inputs.push({description:'d',ws:{url}});
 for (const ws of [{},{url:1},{url:null},{url:'wss://example.com',extra:1},{url:'wss://example.com',protocols:[]},{url:'wss://example.com',protocols:['p','p']},{url:'wss://example.com',protocols:['','p p',1]}, {url:'wss://example.com',protocols:null},{url:'bad',protocols:['bad token','bad token']},{url:'bad',protocols:[1]}]) inputs.push({description:'d',ws});
 inputs.push({...base,timeout_ms:3600001,persistent:true},{description:'d',timeout_ms:3600001},{description:1,command:'bad\u0000',timeout_ms:0},{...base,timeout_ms:0,extra:1},{description:'d',ws:{url:'wss://example.com',protocols:['','']}});
 for (const input of inputs) {
  const result=schema.safeParse(input);const item={mode,input,success:result.success};
  if(result.success)item.normalized=result.data;
  else {ctx.validationError=result.error;item.raw=result.error.message;item.display=vm.runInContext('Yge("Monitor",validationError)',ctx);item.issues=result.error.issues;}
  cases.push(item);
 }
}
ctx.da='Monitor';ctx.bounded=false;
vm.runInContext(between(monitor,'var xe=',',Re=At(')+';',ctx);
ctx.result={taskId:'m12345678',timeoutMs:1000.5,persistent:false};
const fractionalResult=vm.runInContext("xe.mapToolResultToToolResultBlockParam(result,'id').content",ctx);
const output=path.resolve(import.meta.dirname,'../fixtures/monitor_schema_2_1_270.json');fs.writeFileSync(output,JSON.stringify({version:'2.1.270',binarySha256,sources,schemas,cases,fractionalResult},null,2)+'\n');console.log(`Generated ${cases.length} Monitor cases: ${cases.filter(c=>!c.success).length} rejected`);
