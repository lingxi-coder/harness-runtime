#!/usr/bin/env node
// Execute hash-pinned shipped helpers; the fixture contains UTF-16 unit arrays.
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import {fileURLToPath} from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const binary = process.env.CLAUDE_CODE_BINARY ?? path.join(os.homedir(), '.local/share/claude/versions/2.1.322');
const bytes = fs.readFileSync(binary);
const nativeSha256 = 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
assert.equal(crypto.createHash('sha256').update(bytes).digest('hex'), nativeSha256);
const source = bytes.toString('latin1');
function range(start, end) {
  return bytes.subarray(start, end).toString('utf8');
}
function until(start, marker) {
  const end = source.indexOf(marker, start);
  assert(end > start, `missing end marker ${marker}`);
  return range(start, end);
}
const sanitizerStart = source.indexOf('var d=', 260468605);
assert(sanitizerStart > 260468605 && sanitizerStart < 260469071);
const sanitizer = until(sanitizerStart, 'function r3n(');
assert(sanitizer.startsWith('var d='), 'sanitizer source offset changed');
const helpers = [
  until(260461603, 'function Oa('),
  until(260462017, 'function m('),
  until(source.indexOf('function m(', 260462017), 'var b='),
  sanitizer,
  until(261124962, 'function ada('),
].join('\n');
const native = vm.runInNewContext(`${helpers}\n({ln, wr})`, {Buffer}, {timeout: 1000});
const unitArray = value => Array.from({length: value.length}, (_, index) => value.charCodeAt(index));
const cases = [
  '', 'mcp__server__tool', 'mcp__server__', 'mcp__server', 'mcp____tool',
  'mcp__s__a__b', 'mcp__\ud800__\ud801', 'mcp__\ud800missing__read',
  'mcp__server\u200b\u2066__read', 'A\ud800B\udc00C💡',
  'A\u00a0B C\u3000D\u2800E\nF\u2028G\u034fH',
  'a'.repeat(1023) + '💡tail', 'a'.repeat(1024) + 'tail',
  '\u200b'.repeat(4095) + '💡visible', '\u200b'.repeat(4096) + 'visible',
  'constructor', 'prototype', '__proto__',
].map((input, index) => {
  const parsed = native.wr(input);
  return {
    name: `source-${index}`, inputUnits: unitArray(input), sanitized: native.ln(input),
    parsed: parsed === null ? null : {
      serverUnits: unitArray(parsed.serverName),
      toolUnits: parsed.toolName === undefined ? null : unitArray(parsed.toolName),
    },
  };
});
const fixture = {version: '2.1.322', nativeSha256, helperRanges: [
  {start:260461603,end:source.indexOf('function Oa(',260461603)},
  {start:260462017,end:source.indexOf('var b=',260462017)},
  {start:sanitizerStart,end:source.indexOf('function r3n(',sanitizerStart)},
  {start:261124962,end:source.indexOf('function ada(',261124962)},
], cases};
const destination = path.join(root, 'crates/mcp/tests/fixtures/mcp_sdk_control_names_2_1_322.json');
if (process.argv.includes('--write-fixture')) fs.writeFileSync(destination, JSON.stringify(fixture, null, 2) + '\n');
else assert.deepEqual(JSON.parse(fs.readFileSync(destination, 'utf8')), fixture);
console.log(JSON.stringify({passed:true,cases:cases.length,nativeSha256,fixture:destination}));
