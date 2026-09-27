// Run the actual native Zod implementation, English locale, tool schemas and
// diagnostic formatter extracted from the official SHA-pinned 2.1.270 binary.
// Usage: node malformed-input-2.1.270.mjs /tmp/lingxi-loop-oracle-2.1.270
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
const ctx=vm.createContext({...z,f:fn=>fn,At:x=>x,ga:'ScheduleWakeup',cle:'<<autonomous-loop-dynamic>>',h0e:'<<autonomous-loop>>',cw:'CronDelete',da:'Monitor',Kg:'TaskStop',See:7,hY:()=>true,iIn:()=>'',hpr:()=>null,_pr:()=>0});
vm.runInContext(read('src_198617341.js').replace(/import\{[^}]*\}from"[^"]*";/g,''),ctx);
const main=read('src_169588164.js');
function between(s,start,end){let a=s.indexOf(start);if(a<0)throw Error(start);let b=s.indexOf(end,a);if(b<0)throw Error(end);return s.slice(a,b)}
vm.runInContext(between(main,'function o0(e=E())','import{isAbsolute as vas'),ctx);
vm.runInContext(between(read('src_169577058.js'),'function aw(','export{'),ctx);
{ const start=main.indexOf('function ewn('); vm.runInContext(main.slice(start,main.indexOf('function ',start+10)),ctx); }
vm.runInContext(between(main,'function Yge(','function i6('),ctx);
vm.runInContext(between(main,'class Pqe extends Error','var BRn=160000'),ctx);
vm.runInContext(between(read('src_185261434.js'),'var a=50,m=f(',',c=f(')+';globalThis.createSchema=m;',ctx);
vm.runInContext(between(read('src_185268442.js'),'var s=f(',',i=f(')+';globalThis.deleteSchema=s;',ctx);
vm.runInContext(between(read('src_185273754.js'),'var a=f(',',s=f(')+';globalThis.listSchema=a;',ctx);
vm.runInContext(between(read('src_165140675.js'),'var qg=new Set(','function Zn('),ctx);
const definitions=[['ScheduleWakeup','qvs',{delaySeconds:120,reason:'r',prompt:'p',noop:false}],['CronCreate','createSchema',{cron:'* * * * *',prompt:'p'}],['CronDelete','deleteSchema',{id:'a'}],['CronList','listSchema',{}]];
const cases=[]; const schemas={};
const values=[null,false,true,0,1.5,'',[],{},'true','false','120','1e2','Infinity','\ufeff120\ufeff','\u0085120\u0085'];
for(const [tool,builder,base] of definitions){
 const schema=vm.runInContext(`${builder}()`,ctx);
 ctx.toolSchema=schema;schemas[tool]=vm.runInContext('wde(toolSchema,{unrepresentable:"throw"})',ctx);
 const fields=Object.keys(schema.shape);
 const inputs=[null,false,0,'',[],{},base,{...base,unexpected:1},JSON.parse('{"10":true,"2":true,"constructor":true,"__proto__":true,"toString":true}')];
 inputs.push(JSON.parse(JSON.stringify(base).slice(0,-1)+(Object.keys(base).length?',':'')+'"__proto__":{"injected":true}}'));
 for(const field of fields){for(const value of values)inputs.push({...base,[field]:value});let absent={...base};delete absent[field];inputs.push(absent)}
 if(tool==='ScheduleWakeup')inputs.push({stop:true},{stop:true,noop:null},{stop:true,delaySeconds:'1e2'},{stop:true,extra:0},{stop:false},{delaySeconds:120,reason:'r',prompt:'p'},{delaySeconds:120,reason:'r'}, {delaySeconds:120,reason:'r',prompt:'p',noop:false,stop:null});
 for(const input of inputs){
  const parsed=schema.safeParse(input);
  ctx.rawInput=input;ctx.currentTool=tool;
  const coerced=vm.runInContext(`(() => {if(rawInput===null || typeof rawInput!=="object" || Array.isArray(rawInput)) return rawInput; const result={...rawInput};if(currentTool==="ScheduleWakeup" && "delaySeconds" in result)result.delaySeconds=TH(result.delaySeconds);if(currentTool==="CronCreate")for(const key of ["recurring","durable"])if(key in result)result[key]=G4(result[key]);return result})()`,ctx);
  const item={tool,input,coerced,success:parsed.success};
  if(parsed.success){item.normalized=parsed.data;if(tool==='ScheduleWakeup'){ctx.callInput=parsed.data;try{await vm.runInContext('ryr.create({permissions:()=>({})}).call(callInput)',ctx)}catch(error){item.callError=error.message}}}
  else {ctx.validationError=parsed.error;ctx.toolName=tool;item.raw=parsed.error.message;item.display=vm.runInContext('Yge(toolName,validationError)',ctx);item.issues=parsed.error.issues}
  cases.push(item);
 }
}
const output=path.resolve(import.meta.dirname,'../fixtures/malformed_input_2_1_270.json');
fs.writeFileSync(output,JSON.stringify({version:'2.1.270',binarySha256,sources,schemas,cases},null,2)+'\n');
console.log(`Generated ${cases.length} actual Zod cases: ${cases.filter(c=>!c.success).length} rejected`);
