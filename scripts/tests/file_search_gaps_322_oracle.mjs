#!/usr/bin/env node
// Exercise the native postfilter and bundled rg without starting Claude accounts
// or model requests. Retain results and byte receipts, never extracted source.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
const args = process.argv.slice(2);
assert(args.every(arg => arg === '--check'));
const binaryPath = path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [{ name: 'Glob ordered Read-deny postfilter', start: 272415401, end: 272416874,
  sha256: 'f6e8af575abf6131b0515de5cdda2e282d72abd060c3617d8e4b2b34302e8c51' }];
const receipt = sourceByteRanges[0];
const source = binary.subarray(receipt.start, receipt.end);
assert.equal(sha256(source), receipt.sha256);
const postfilters = [];
for (const input of [
  { name: 'inactive exact total', active: false, files: ['a','b','c','d'], limit: 3 },
  { name: 'active floor', active: true, files: ['a','b','c','d'], limit: 3 },
  { name: 'active exact page fills at end', active: true, files: ['a','b','c'], limit: 3 },
  { name: 'active trailing denied still incomplete', active: true, files: ['a','b','c','deny'], denied: ['deny'], limit: 3 },
  { name: 'active leading denied exhausts stream', active: true, files: ['deny','a','b','c'], denied: ['deny'], limit: 3 },
  { name: 'active permitted page shorter than limit', active: true, files: ['a','deny'], denied: ['deny'], limit: 3 },
  { name: 'offset contributes to permitted floor', active: true, files: ['a','b','c','d','e'], offset: 1, limit: 3 },
]) {
  const context = vm.createContext({
    INPUT: input, process: { env: {} }, gon: () => new Map(input.active ? [['deny', true]] : []),
    cTr: () => [], Pe: () => true, iRe: async () => [],
    qX: async () => input.files, Ayo: files => files, vyo: () => false, HUn: () => false,
    X2i: file => file, NUn: () => true,
    Ixr: files => new Set(files.filter(file => (input.denied ?? []).includes(file))),
    Tyo: file => file,
  });
  vm.runInContext(source.toString('utf8'), context);
  const expected = await vm.runInContext(`Par({canonical:'/fixture',target:'/fixture',isDirectory:true,
    spawnCwd:'/fixture',judgeEveryResult:true,recheckBeforeSpawn(){}},'**/*',
    {limit:INPUT.limit,offset:INPUT.offset??0},undefined,{})`, context);
  postfilters.push({ input, expected: JSON.parse(JSON.stringify(expected)) });
}
const rg = argv => {
  const result = spawnSync(binaryPath, argv, { env: { ...process.env, ARGV0: 'rg' },
    encoding: 'utf8', timeout: 15000, maxBuffer: 8 * 1024 * 1024 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
};
const typeList = rg(['--type-list']);
const types = typeList.trimEnd().split('\n').map(line => {
  const at = line.indexOf(': '); return { name: line.slice(0,at), globs: line.slice(at+2).split(', ') };
});
assert.equal(readFileSync(new URL('../../crates/tools/file/src/templates/claude-code-2.1.322/rg-types.txt', import.meta.url), 'utf8'), typeList);
const fixtureDir = mkdtempSync(path.join(tmpdir(), 'search-gaps-322-'));
let largePage;
try {
  const file = path.join(fixtureDir, 'large.txt');
  writeFileSync(file, Array.from({length:10005},(_,i)=>`hit${i}\n`).join(''));
  const lines = rg(['--json','hit',file]).trimEnd().split('\n').map(JSON.parse)
    .filter(event => event.type === 'match').map(event => `${event.data.line_number}:${event.data.lines.text.trimEnd()}`);
  largePage = { total: lines.length, offset: 10001, limit: 3, page: lines.slice(10001,10004), last: lines.at(-1) };
  assert.equal(largePage.total, 10005);
} finally { rmSync(fixtureDir, {recursive:true,force:true}); }
// Native Grep calls String.localeCompare with no explicit locale/options. The
// installed Bun matches the native embedder; this is controlled-runtime evidence
// for that language primitive, separate from the bundled rg execution above.
const bunVersion = spawnSync('bun',['--version'],{encoding:'utf8',timeout:5000});
assert.equal(bunVersion.status,0); assert.equal(bunVersion.stdout.trim(),'1.4.3');
const names = ['file2','file10','foo-2','foo_2','ä','ae','a','A','é','e\u0301','i','İ','ı','😀','skill-🧪-probe','skill.alpha+beta'];
const collationResult = spawnSync('bun',['-e',`const names=${JSON.stringify(names)};console.log(JSON.stringify({locale:new Intl.Collator().resolvedOptions().locale,
  en:names.toSorted((a,b)=>a.localeCompare(b,'en-US')),sv:names.toSorted((a,b)=>a.localeCompare(b,'sv-SE'))}))`], {encoding:'utf8',timeout:5000});
assert.equal(collationResult.status,0,collationResult.stderr);
const collation = {runtime:'Bun 1.4.3',input:names,...JSON.parse(collationResult.stdout)};
// Replay the native argument builder, event parser and result projection with
// actual bundled-rg output. Store exact input JSON inside an ordinary JSON
// string so the Rust serde fixture reader never repairs/rejects source units.
const grepRanges = [
  ['incomplete search notice',272417437,272417614,'21928496d807844d29e6eb75c3b1e411e24bdb21b458cab5499edbf428438c45'],
  ['head limit and pagination',272427250,272427522,'0337a4210ad22846f5c46af9f41ce9d3ec777503a9900e12b422055c2ede252b'],
  ['grep result mapper',272428949,272429990,'2c63bf54f1d41f3e58fde102d9737f726528c0471db6d6a4c249d56fcba9d52c'],
  ['ripgrep JSON event parser and line offset',272437708,272439143,'065088479b8a096cd28db5a129e287697c9ac6ada984f48a1b310065e745c40b'],
  ['grep call result and search arguments',272431723,272437053,'9439a554d7f647124784fff88f1a0d46fe12b83d7c7b2041d5c96cd352a042ad'],
  ['grep lexical result paths',272437053,272437415,'a4693203413cda874aba360e9752a0391524fab185749c920d3987e2abdb2b35'],
  ['one suffix removal',260463926,260463995,'ebc95b8712d8d2875533b7d0885bc7cf4386ef983718267912d7e3fc42955774'],
];
const [timeoutNotice,pagination,grepMapper,contentParser,contentSearch,lexicalPaths,suffix] = grepRanges.map(([name,start,end,digest])=>{
  const bytes=binary.subarray(start,end);assert.equal(sha256(bytes),digest);
  sourceByteRanges.push({name,start,end,sha256:digest});return bytes.toString('utf8');
});
const surrogateSearches=[];
for(const spec of [
  {name:'high surrogate regex becomes replacement',contents:'�\n',options:{pattern:'\ud800'},singleFile:true},
  {name:'low surrogate regex becomes replacement',contents:'�\n',options:{pattern:'\udfff'},singleFile:true},
  {name:'high surrogate glob is retained',contents:'hit\n',options:{pattern:'hit',glob:'\ud800'}},
  {name:'wildcard surrogate glob is retained',contents:'hit\n',options:{pattern:'hit',glob:'*\ud800*'}},
]){
  const directory=mkdtempSync(path.join(tmpdir(),'search-surrogate-322-'));
  try{
    const file=path.join(directory,'a.txt');writeFileSync(file,spec.contents);
    const root='/fixture';const options={output_mode:'content',...spec.options};
    const context=vm.createContext({Buffer,JSON,ps:JSON.parse,zar:250,Gar:['.git','.svn','.hg','.bzr','.jj','.sl'],
      gon:()=>new Map(),cTr:()=>[],vyo:()=>false,HUn:()=>false,iRe:async()=>[],x$n:()=>false,FAs:()=>[],WW:'/',
      qX:async argv=>{
        const result=spawnSync(binaryPath,[...argv,spec.singleFile?file:directory],{cwd:directory,env:{...process.env,ARGV0:'rg'},encoding:'utf8',timeout:15000,maxBuffer:1024*1024});
        assert.ifError(result.error);assert([0,1].includes(result.status),result.stderr);
        return result.stdout.split('\n').filter(Boolean).map(line=>{const e=JSON.parse(line);
          if(e.data?.path?.text?.startsWith(directory+path.sep))e.data.path.text=root+'/'+e.data.path.text.slice(directory.length+1);
          return JSON.stringify(e);});
      },wpn:name=>name.startsWith(root+'/')?name.slice(root.length+1):name,
      P:(n,word)=>n===1?word:word+'s',NUn:()=>false,
      ROOT:spec.singleFile?root+'/a.txt':root,SINGLE_FILE:!!spec.singleFile,OPTIONS:options,
    });
    vm.runInContext(timeoutNotice+pagination+suffix+contentParser+contentSearch+lexicalPaths+`;var grep={${grepMapper}};`,context);
    const result=await vm.runInContext(`Xar({canonical:ROOT,lexical:ROOT,target:ROOT,isDirectory:!SINGLE_FILE,relativeOutput:false,spawnCwd:ROOT,recheckBeforeSpawn(){}},OPTIONS,{permissions:()=>({})})`,context);
    context.RESULT=result;const mapped=vm.runInContext(`grep.mapToolResultToToolResultBlockParam(RESULT.data,'tool-1')`,context);
    surrogateSearches.push({name:spec.name,contents:spec.contents,inputJson:JSON.stringify(options),singleFile:!!spec.singleFile,expected:JSON.parse(JSON.stringify(result.data)),modelText:mapped.content});
  }finally{rmSync(directory,{recursive:true,force:true});}
}
const surrogateGlobs=[];
for(const pattern of ['\ud800.txt','*\ud800*','\udc00.txt']){
  const directory=mkdtempSync(path.join(tmpdir(),'glob-surrogate-322-'));
  try{
    for(const name of ['�.txt','ordinary.txt'])writeFileSync(path.join(directory,name),'x');
    const context=vm.createContext({process:{env:{}},gon:()=>new Map(),cTr:()=>[],Pe:()=>true,iRe:async()=>[],
      qX:async argv=>{const result=spawnSync(binaryPath,[...argv,directory],{env:{...process.env,ARGV0:'rg'},encoding:'utf8',timeout:15000,maxBuffer:1024*1024});
        assert.ifError(result.error);assert.equal(result.status,0,result.stderr);
        return result.stdout.split('\0').filter(Boolean).map(name=>name.replace(directory,'/fixture'));},
      Ayo:files=>files,vyo:()=>false,HUn:()=>false,X2i:file=>file,NUn:()=>false,
      PATTERN:pattern});
    vm.runInContext(source.toString('utf8'),context);
    const expected=await vm.runInContext(`Par({canonical:'/fixture',target:'/fixture',isDirectory:true,spawnCwd:'/fixture',recheckBeforeSpawn(){}},PATTERN,{limit:100,offset:0},undefined,{})`,context);
    surrogateGlobs.push({inputJson:JSON.stringify({pattern}),expected:JSON.parse(JSON.stringify(expected))});
  }finally{rmSync(directory,{recursive:true,force:true});}
}
const typeDirectory=mkdtempSync(path.join(tmpdir(),'type-surrogate-322-'));
let surrogateType;
try{
  const file=path.join(typeDirectory,'a.txt');writeFileSync(file,'hit\n');const type='\ud800';
  const result=spawnSync(binaryPath,['--json','--regexp','hit','--type',type,file],{env:{...process.env,ARGV0:'rg'},encoding:'utf8',timeout:15000});
  assert.ifError(result.error);assert.equal(result.status,2);
  surrogateType={inputJson:JSON.stringify({pattern:'hit',type,output_mode:'content'}),status:result.status,stderr:result.stderr};
}finally{rmSync(typeDirectory,{recursive:true,force:true});}

const fixture = {schemaVersion:1,claudeCodeVersion:'2.1.322',nativeSha256,sourceByteRanges,
  postfilters,typesSha256:sha256(typeList),types,largePage,collation,surrogateSearches,surrogateGlobs,surrogateType};
const fixturePath = new URL('./fixtures/file_search_gaps_322.json', import.meta.url);
const output = JSON.stringify(fixture,null,2)+'\n';
if(args.includes('--check')) assert.equal(readFileSync(fixturePath,'utf8'),output); else writeFileSync(fixturePath,output);
console.log(`Claude Code 2.1.322 search gaps: ${postfilters.length} postfilters, ${types.length} type definitions, ${largePage.total} rg rows, 2 Bun locale orders, ${surrogateSearches.length+surrogateGlobs.length+1} malformed-string argument cases`);
