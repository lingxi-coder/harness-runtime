#!/usr/bin/env node
// Immutable shipped-source replay. No git, credentials, network or browser.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
const root = path.resolve(import.meta.dirname, '../..');
const binaryPath = process.env.CLAUDE_322_BINARY ?? '/Users/luolingfeng/.local/share/claude/versions/2.1.322';
const expectedSha = 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
const binary = fs.readFileSync(binaryPath);
assert.equal(crypto.createHash('sha256').update(binary).digest('hex'), expectedSha);
const sourceDir = process.env.WORKTREE_322_ARTIFACT_DIR ?? '/Users/luolingfeng/lingxi/artifacts/mcp-gap-closure-322-20261109/work/worktree';
fs.mkdirSync(path.join(sourceDir, 'valid-source'), {recursive: true});
const receipts = [];
// nr is a Bun runtime intrinsic. Node delegates that one dependency to the
// installed Bun instead of assuming Node's terminal parser is identical.
const taskBun = process.env.WORKTREE_322_BUN ?? '/Users/luolingfeng/.bun/bin/bun';
const stripNative = typeof Bun === 'undefined'
  ? value => JSON.parse(execFileSync(taskBun, ['-e', 'process.stdout.write(JSON.stringify(Bun.stripANSI(JSON.parse(await Bun.stdin.text()))))'], {input:JSON.stringify(value),encoding:'utf8'}))
  : value => Bun.stripANSI(value);

function extract(name, start, marker) {
  const end = binary.indexOf(Buffer.from(marker), start);
  assert.ok(end > start && end-start < 20000, name);
  const bytes = binary.subarray(start,end);
  const source = bytes.toString('utf8');
  new vm.Script(source);
  fs.writeFileSync(path.join(sourceDir, 'valid-source', `${name}.js`), source);
  receipts.push({name, start, end, length:end-start, sha256:crypto.createHash('sha256').update(bytes).digest('hex')});
  return source;
}
const selectSource = extract('VRo-uK',270906766,'function kme(');
const createSource = extract('Jyr',274581115,'async function Xlo(');
const relativeSource = extract('qUe',261771523,'function bl(');
const removeSource = extract('Xlo',274582423,'var eIi=');
const stripSource = extract('nr',260473744,'function B4e(');
const personalSource = extract('Fpt',274634020,'function lG(');
const restrictionSource = extract('Zy',265300699,'var t=300;');
const diagnosticBody=fs.readFileSync(path.join(root,'crates/hooks/src/worktree_manager/native_display.js'),'utf8');
const diagnostic=new vm.Script(`(function(input){${diagnosticBody}})`).runInNewContext({Buffer});

extract('cU',274700334,'function ndo(');
extract('XI',269631609,'function TZt(');

const createCases = [
  {id:'not-configured',configured:false,results:[]},
  {id:'configured-not-run',results:[]},
  {id:'empty-success',results:[{succeeded:true,output:''}]},
  {id:'blank-success',results:[{succeeded:true,output:' \n\r\t\n'}]},
  {id:'last-nonempty-line',results:[{succeeded:true,output:'preparing\n /selected \n\n'}]},
  {id:'ansi-path',results:[{succeeded:true,output:'progress\n\u001b[31m/selected\u001b[0m\n'}]},
  {id:'osc-link-path',results:[{succeeded:true,output:'\u001b]8;;https://invalid.test\u0007/selected\u001b]8;;\u0007'}]},
  {id:'feff-and-wide-space',results:[{succeeded:true,output:'\ufeff\u3000/selected\u3000\ufeff'}]},
  {id:'relative',results:[{succeeded:true,output:'feature'}]},
  {id:'relative-dot-normalized',results:[{succeeded:true,output:'./nested/../feature'}]},
  {id:'parent-relative',results:[{succeeded:true,output:'../feature'}]},
  {id:'live-local-cwd',cwd:'/live/current',results:[{succeeded:true,output:'feature'}]},
  {id:'remote-original-cwd',cwd:'/peer/current',remote:true,results:[{succeeded:true,output:'feature'}]},
  {id:'absolute-spelling-retained',results:[{succeeded:true,output:'/a/../selected'}]},
  {id:'first-hook-wins',results:[{succeeded:true,output:'/first'},{succeeded:true,output:'/later'}]},
  {id:'failure-then-success',results:[{succeeded:false,output:'failed',command:'create-one'},{succeeded:true,output:'/later'}]},
  {id:'blank-then-success',results:[{succeeded:true,output:''},{succeeded:true,output:'/later'}]},
  {id:'held-then-managed',results:[{succeeded:true,answerHeld:true,output:'/held'},{succeeded:true,output:'/managed'}]},
  {id:'held-only',results:[{succeeded:true,answerHeld:true,output:'/held'}]},
  {id:'failed-before-held',results:[{succeeded:false,command:'create-one',output:'failed'},{succeeded:true,answerHeld:true,output:'/held'}]},
  {id:'failed-no-output',results:[{succeeded:false,command:'create-one',output:' \n'}]},
  {id:'failed-multiline-preserved',results:[{succeeded:false,command:'create-one',output:' first\nsecond\n '}]},
  {id:'failed-middle-truncation',results:[{succeeded:false,command:'create-one',output:'a'.repeat(12000)}]},
  {id:'multiple-failures',results:[{succeeded:false,command:'one',output:'first'},{succeeded:false,command:'two',output:'second'}]},
  {id:'success-ignores-blocked',results:[{succeeded:true,blocked:true,output:'/selected'}]},
  {id:'lone-utf16',results:[{succeeded:true,output:'/selected\ud800'}]},
  {id:'unicode-line-separator-not-newline',results:[{succeeded:true,output:'a\u2028/selected'}]},
  ...[
    ['dcs','\u001bPprogress\u001b\\/selected'],
    ['c1-dcs','\u0090progress\u009c/selected'],
    ['sos','\u001bXprogress\u001b\\/selected'],
    ['pm','\u001b^progress\u001b\\/selected'],
    ['apc','\u001b_progress\u001b\\/selected'],
    ['dcs-unclosed','\u001bPprogress\u0007/selected'],
    ['osc-unclosed','\u001b]unfinished'],
    ['csi-cancel','\u001b[1\u0018m/selected'],
    ['csi-substitute','\u001b[1\u001am/selected'],
    ['nested-escape','\u001b[1\u001b[31m/selected'],
    ['escape-unicode','\u001b😀/selected'],
    ['escape-linefeed','\u001b\n31m/selected'],
    ['escape-intermediate','\u001b(0/selected'],
    ['csi-unicode','\u001b[1😀m/selected'],
    ['c1-st-ground','\u009c/selected'],
    ['osc-cancel','\u001b]abc\u0018/selected'],
    ['dcs-substitute','\u001bPabc\u001a/selected'],
    ['escape-c1-csi','\u001b\u009b31m/selected'],
    ['csi-c1-osc-ignored','\u001b[1\u009d8;;url\u0007/selected'],
    ['escape-cancel','\u001b\u0018/selected'],
    ['intermediate-cancel','\u001b(\u0018/selected'],
    ['csi-c1-dcs-ignored','\u001b[1\u0090abc\u009c/selected'],
  ].map(([id,output])=>({id,results:[{succeeded:true,output}]})),
];
const removeCases = [
  {id:'not-configured',configured:false,results:[]},
  {id:'configured-not-run',results:[]},
  {id:'success',results:[{succeeded:true,output:''}]},
  {id:'success-untrusted-present',untrusted:true,results:[{succeeded:true,output:''}]},
  {id:'success-untrusted-gone',untrusted:true,gone:true,results:[{succeeded:true,output:''}]},
  {id:'failure-present',results:[{succeeded:false,exitCode:7,output:'denied'}]},
  {id:'failure-gone',gone:true,results:[{succeeded:false,exitCode:7,output:'denied'}]},
  {id:'failure-no-exit',results:[{succeeded:false,output:'denied'}]},
  {id:'failure-quoted-normalized',results:[{succeeded:false,exitCode:4,output:'"Cafe\u0301" \u2018denied\u2019'}]},
  {id:'failure-head-tail-truncation',results:[{succeeded:false,exitCode:4,output:'a'.repeat(100)+'middle'+'z'.repeat(100)}]},
  {id:'failure-control-only-line-skipped',results:[{succeeded:false,exitCode:4,output:'\u001b[31m\u001b[0m\nprintable'}]},
  {id:'failure-empty',results:[{succeeded:false,output:' \n\t'}]},
  {id:'failure-first-nonempty-line',results:[{succeeded:false,exitCode:9,output:'\n\nfirst\nsecond'}]},
  {id:'multiple-failures-first-wins',results:[{succeeded:false,exitCode:1,output:'one'},{succeeded:false,exitCode:2,output:'two'}]},
  {id:'success-before-failure',results:[{succeeded:true,output:''},{succeeded:false,exitCode:3,output:'later'}]},
];
function evaluate(test, operation) {
  const results = structuredClone(test.results).map(result=>({command:'callback',...result}));
  const state = {configured:test.configured!==false,results};
  const context = {
    // Bundled nr calls the runtime intrinsic. Node delegates only that
    // dependency to the installed Bun; the shipped JS executes in the VM.
    Bun: {stripANSI:stripNative},
    Ip:()=>({cwd:test.cwd??'/original/repo'}),pe:()=>test.cwd??'/original/repo',cU:async()=>results,
    Cu:600000, // Shipped command timeout binding @269243553; runner is controlled.
    z_e:()=>state.configured,P9r:()=>state.configured,apo:()=>state.configured,
    A9r:path.isAbsolute,Wo:()=>({originalCwd:'/original/repo'}),vt:()=>test.remote===true,
    tdo:async cwd=>cwd,x9r:path.resolve,T:Error,
    Mu:value=>diagnostic({phase:'create',text:value}),Kd:value=>diagnostic({phase:'remove',text:value}),t:()=>{},y2:()=>test.untrusted===true,
    lN:async()=>test.gone?'gone':'present',M9r:60,O9r:100,
  };
  vm.createContext(context);
  new vm.Script(`${stripSource};${selectSource};${relativeSource};${createSource};${removeSource};globalThis.run=${operation==='create'?'Jyr':'Xlo'};`).runInContext(context);
  return context.run({},operation==='create'?'native':'/owned/tree').then(value=>value===undefined?{kind:'none'}:JSON.parse(JSON.stringify(value))).catch(error=>({error:error.message}));
}
const document = {
  version:'2.1.322',binarySha256:expectedSha,
  create:await Promise.all(createCases.map(async test=>({...test,expected:await evaluate(test,'create')}))),
  remove:await Promise.all(removeCases.map(async test=>({...test,expected:await evaluate(test,'remove')}))),
};
// A missing VM dependency must fail the oracle, never become a fixture.
for(const test of [...document.create,...document.remove]) {
  assert.ok(!/\b(?:is not defined|is not a function)\b/.test(test.expected.error??''), `${test.id}: ${JSON.stringify(test.expected)}`);
}
assert.deepEqual(document.create.find(test=>test.id==='last-nonempty-line').expected,{worktreePath:'/selected'});
assert.deepEqual(document.remove.find(test=>test.id==='success').expected,{kind:'removed'});
const fixturePath=path.join(root,'crates/hooks/tests/fixtures/worktree_hooks_2_1_322.json');
if(process.argv.includes('--write-fixture'))fs.writeFileSync(fixturePath,JSON.stringify(document,null,2)+'\n');
else assert.deepEqual(document,JSON.parse(fs.readFileSync(fixturePath,'utf8')));
const summary={version:'2.1.322',binarySha256:expectedSha,runtime:typeof Bun==='undefined'?`Node ${process.version}`:`Bun ${Bun.version}`,cases:createCases.length+removeCases.length,sourceExecution:true,wholeCli:false,adapters:['hook runner returns controlled classic results','local session cwd/project resolution','path-presence and workspace trust state','Node only: nr runtime intrinsic delegated to installed Bun'],receipts};
fs.writeFileSync(path.join(sourceDir,`summary-${typeof Bun==='undefined'?'node':'bun'}.json`),JSON.stringify(summary,null,2)+'\n');
fs.writeFileSync(path.join(sourceDir,'ranges.json'),JSON.stringify(receipts,null,2)+'\n');
console.log(JSON.stringify({cases:summary.cases,runtime:summary.runtime,binarySha256:expectedSha,status:'passed'}));
