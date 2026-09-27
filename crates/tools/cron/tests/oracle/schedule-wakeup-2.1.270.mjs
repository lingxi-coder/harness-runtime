// Execute functions extracted verbatim from the official 2.1.270 native bundle.
// Usage: node schedule-wakeup-2.1.270.mjs /tmp/lingxi-loop-oracle-2.1.270
// No Anthropic code is checked in; the generated fixture contains observable outputs.
import fs from 'node:fs';
import vm from 'node:vm';
import crypto from 'node:crypto';
import path from 'node:path';
const root = process.argv[2];
const hash = crypto.createHash('sha256').update(fs.readFileSync(path.join(root, 'package/claude'))).digest('hex');
if (hash !== 'a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807') throw Error('Unexpected oracle binary');
const read = name => fs.readFileSync(path.join(root, 'chunks', name), 'utf8');
const main = read('src_169588164.js');
const prompt = read('src_168809137.js');
const runtime = read('src_169499617.js');
const ctx = vm.createContext({Date, Math, Number});
vm.runInContext(prompt.slice(prompt.indexOf('var ga='), prompt.indexOf('var uS=')), ctx);
vm.runInContext(main.slice(main.indexOf('function TH('), main.indexOf('import{isAbsolute as vas')), ctx);
// Schema builder stand-ins only record the literal type/description/optionality;
// functions, prompt templates, runtime timing and result mapping execute unchanged.
vm.runInContext(`
function node(type) {return {type,optional(){this.opt=true;return this},describe(description){this.description=description;return this}}}
function E(){return node('number')} function o(){return node('string')} function I(){return node('boolean')}
function u(properties){return {type:'object',properties:Object.fromEntries(Object.entries(properties).map(([k,v])=>[k,{type:v.type,description:v.description}])),required:Object.entries(properties).filter(([k,v])=>!v.opt).map(([k])=>k)}}
function Qe(p){let s=u(p);s.additionalProperties=false;if(!s.required.length)delete s.required;return s}
function f(fn){return fn} function o0(e){return e} function At(e){return e}
const cw='CronDelete',da='Monitor',Kg='TaskStop';
function _pr(){return 0} function hpr(){return null}
`,ctx);
vm.runInContext(main.slice(main.indexOf('class Pqe extends Error'),main.indexOf('var BRn=160000')),ctx);
vm.runInContext(`const g=60,L=3600,qxt=300000;function Dae(){return {cacheLeadMs:15000}}`,ctx);
vm.runInContext(runtime.slice(runtime.indexOf('function P(e)'),runtime.indexOf('function x()')),ctx);
const now = 1800000000123;
vm.runInContext(`Date=class extends Date {static now(){return ${now}}}`,ctx);
const fixture = {version:'2.1.270',binarySha256:hash,sourceSha256:Object.fromEntries([
 ['src_169588164.js',main],['src_168809137.js',prompt],['src_169499617.js',runtime]
].map(([name,source])=>[name,crypto.createHash('sha256').update(source).digest('hex')])),nowMs:now,
 inputSchema:JSON.parse(fs.readFileSync(path.resolve(import.meta.dirname,'../fixtures/malformed_input_2_1_270.json'),'utf8')).schemas.ScheduleWakeup,outputSchema:vm.runInContext('Vvs()',ctx),
 prompts:{oneHour:vm.runInContext('bgr(true)',ctx),fiveMinutes:vm.runInContext('bgr(false)',ctx),unknown:vm.runInContext('bgr(undefined)',ctx)},
 timing:[],coercion:[],results:[],errors:[]};
for(const offset of [0, 1, 14999, 15000, 30000, 59999]) {
 const at = Math.floor(now / 60000) * 60000 + offset;
 vm.runInContext(`Date=class extends Date {static now(){return ${at}}}`,ctx);
for(const raw of ['-Infinity','NaN','Infinity','-1','0','59.49','59.5','60','60.5','269.5','270','285','299.5','300','300.5','3599.5','3600','3600.5','9999']) {
 const t=vm.runInContext(`P(${raw})`,ctx);fixture.timing.push({raw,nowMs:at,clamped:t.clamped,wasClamped:t.wasClamped,targetMs:t.targetMs});
}
}
vm.runInContext(`Date=class extends Date {static now(){return ${now}}}`,ctx);
for(const value of ['120',' +120.5 ','1e2','.5','1.','0x10','Infinity','NaN','','  ','١٢','120\n','\ufeff120\ufeff','\u0085120\u0085']) {
 ctx.value=value;const v=vm.runInContext('TH(value)',ctx);fixture.coercion.push({input:value,accepted:typeof v==='number',value:typeof v==='number'?v:null});
}
for(const result of [{scheduledFor:0},{stopped:true,cancelledWakeups:0},{stopped:true,cancelledWakeups:2},{scheduledFor:now+90000,clampedDelaySeconds:60,wasClamped:false},{scheduledFor:now+90000,clampedDelaySeconds:60,wasClamped:true}]) {
 ctx.result=result;fixture.results.push({input:result,content:vm.runInContext("ryr.mapToolResultToToolResultBlockParam(result,'id').content",ctx)});
}
for(const input of [{},{delaySeconds:60,reason:'r'},{delaySeconds:60,reason:'r',prompt:'p'}]) {
 ctx.input=input;try{await vm.runInContext('ryr.create({permissions:()=>({})}).call(input)',ctx)}catch(e){fixture.errors.push({input,message:e.message})}
}
// Execute the native formatter under independent IANA timezone settings.
fixture.localResults = {};
const originalTZ = process.env.TZ;
for (const zone of ['UTC', 'America/Los_Angeles', 'Asia/Shanghai', 'Asia/Kathmandu', 'America/St_Johns', 'Australia/Lord_Howe']) {
 process.env.TZ = zone;
 fixture.localResults[zone] = [];
 for (const at of [now, Date.UTC(2026,0,15), Date.UTC(2026,6,15), Date.UTC(2026,2,8,9,59,30)]) {
  vm.runInContext(`Date=class extends Date {static now(){return ${at}}}`,ctx);
  for (const clamped of [false, true]) {
   const result = {scheduledFor:at+90000,clampedDelaySeconds:60,wasClamped:clamped};
   ctx.result=result;
   fixture.localResults[zone].push({nowMs:at,input:result,content:vm.runInContext("ryr.mapToolResultToToolResultBlockParam(result,'id').content",ctx)});
  }
 }
}
if (originalTZ === undefined) delete process.env.TZ; else process.env.TZ = originalTZ;
const output = path.resolve(import.meta.dirname,'../fixtures/schedule_wakeup_2_1_270.json');
fs.writeFileSync(output,JSON.stringify(fixture,null,2)+'\n');
console.log(`Generated ${output}: ${fixture.timing.length} timings, ${fixture.coercion.length} coercions, 3 prompts, 2 schemas, ${fixture.results.length} results, ${fixture.errors.length} errors`);
