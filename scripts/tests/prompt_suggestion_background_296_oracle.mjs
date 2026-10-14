#!/usr/bin/env node
// No provider call. Check retained helper functions against shipped bytes and
// run the production guest program using Node's native ECMAScript semantics.
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import {createRequire} from 'node:module';
const require=createRequire(new URL('../../../lingxi-app/apps/electron/package.json',import.meta.url));
const parser=require('@babel/parser');
const traverse=require('@babel/traverse').default;
const root=new URL('../../',import.meta.url);
const directory=new URL('crates/orchestrator/src/conversation/',root);
const read=name=>fs.readFileSync(new URL(name,directory),'utf8');
const binary=fs.readFileSync(process.argv[2]??`${process.env.HOME}/.local/share/claude/versions/2.1.296`);
assert.equal(crypto.createHash('sha256').update(binary).digest('hex'),'c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937');
let functions=0;
for(const name of ['prompt_suggestion_classifier_native.js','prompt_suggestion_redactor_native.js','prompt_suggestion_background_tail_native.js','prompt_suggestion_background_input_native.js']) {
  const source=read(name),tree=parser.parse(source);
  traverse(tree,{FunctionDeclaration(path){
    const text=source.slice(path.node.start,path.node.end);
    assert.ok(binary.includes(Buffer.from(text)),`${name}: native function ${path.node.id.name} no longer matches shipped source bytes`);
    functions++;
  }});
}
const program=read('prompt_suggestion_redactor_native.js')+'\nconst classifier=(()=>{'+read('prompt_suggestion_classifier_adapter.js')+read('prompt_suggestion_classifier_native.js')+';return {wZr,Oxn,Hxn,vZr,brs,wrs};})();\n'+read('prompt_suggestion_background_tail_native.js')+'\n'+read('prompt_suggestion_background_input_native.js')+'\n(()=>{'+read('prompt_suggestion_background_adapter.js')+'})()';
const run=input=>JSON.parse(JSON.stringify(vm.runInNewContext(program,{input})));
for(const text of ['Which option do you prefer?','Result: Done.\nNext: more work','Needs input: choose one','I am waiting for your approval.','```\nWhich option?\n```','🎉 ...','Use ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa']) {
  const output=run({operation:'prepare',text,previous:'working',latestAsk:'Continue',tools:'',minutes:0});
  assert.equal(typeof output.prompt,'string');
}
assert.equal(run({operation:'safeReply',text:'Use ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'}),false);
assert.equal(run({operation:'safeReply',text:'Use the first option'}),true);
// Prompt module source is a Bun chunk containing template literals; evaluate the
// actual exported locals rather than comparing escaped source to rendered text.
const marker=Buffer.from('A user kicked off a Claude Code agent');
const at=binary.indexOf(marker);assert.ok(at>=0);
const start=binary.lastIndexOf(Buffer.from('// @bun @bytecode'),at),end=binary.indexOf(0,at);
const chunk=binary.subarray(start,end).toString('utf8');
const declarations=chunk.slice(chunk.indexOf('var e='),chunk.lastIndexOf('export{'));
const prompts=vm.runInNewContext(`${declarations};({e,n})`);
assert.equal(read('prompt_suggestion_classifier_prompt.txt'),prompts.e);
assert.equal(read('prompt_suggestion_classifier_leading_ask.txt'),prompts.n);
console.log(JSON.stringify({version:'2.1.296',nativeFunctions: functions,promptBytes:Buffer.byteLength(prompts.e),leadingAskBytes:Buffer.byteLength(prompts.n),status:'passed'}));
