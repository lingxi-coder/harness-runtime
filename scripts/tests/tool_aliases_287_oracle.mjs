#!/usr/bin/env node
// Current native name normalization, without older Harness aliases.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const digest = data => createHash('sha256').update(data).digest('hex');
const nativeSha256 = digest(binary);
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const start = binary.indexOf('var c={Task:"Agent",KillShell:', 178262885);
const end = binary.indexOf('var AAn=', start);
assert(start >= 178262885 && end > start);
const bytes = binary.subarray(start, end);
const native = vm.runInNewContext(`(()=>{${bytes};return {normalize:Zl,aliases:c};})()`);
assert.equal(Object.keys(native.aliases).length, 8);
const names = [...Object.keys(native.aliases), 'AgentOutputTool', 'BashOutputTool',
  'AgentOutput', 'BashOutput', 'TaskOutput', 'Read', '__proto__', 'constructor',
  'toString', '', 'mcp__custom__tool'];
const cases = names.map(input => ({ input, expected: native.normalize(input) }));
const output = {version:'2.1.287',native_sha256:nativeSha256,
  sourceByteRanges:[{start,end,sha256:digest(bytes)}],
  scope:'Exact current native c/Zl/TZe table and Object.hasOwn normalization. This proves name normalization; catalog membership and tool execution are separate callers.',
  aliases:JSON.parse(JSON.stringify(native.aliases)),cases};
const file = fileURLToPath(new URL('../../crates/llm-runtime/tests/fixtures/tool_aliases_2_1_287.json',import.meta.url));
const json = JSON.stringify(output,null,2)+'\n';
if(process.argv.includes('--check')) assert.equal(readFileSync(file,'utf8'),json);
else writeFileSync(file,json);
process.stdout.write(`${process.argv.includes('--check')?'checked':'wrote'} native 2.1.287 tool-alias oracle: ${cases.length} cases, 8 aliases\n`);
