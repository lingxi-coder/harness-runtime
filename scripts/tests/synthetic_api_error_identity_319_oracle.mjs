#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: synthetic_api_error_identity_319_oracle.mjs <official 2.1.319 binary> [--check]');
const fd = fs.openSync(binaryPath, 'r');
const digest = createHash('sha256'), block = Buffer.alloc(1024 * 1024);
for (let at = 0, size; (size = fs.readSync(fd, block, 0, block.length, at)); at += size) digest.update(block.subarray(0, size));
const nativeSha256 = digest.digest('hex');
assert.equal(nativeSha256, '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd');
const base = 269336381, chunk = Buffer.alloc(274727937 - base);
assert.equal(fs.readSync(fd, chunk, 0, chunk.length, base), chunk.length);
fs.closeSync(fd);
const begin = chunk.indexOf('function Lpo({content:');
const end = chunk.indexOf('function Yct(e)', begin);
assert(begin >= 0 && end > begin, 'Missing actual Native assistant factories');
const source = chunk.subarray(begin, end);
const state = {allocations: []};
const uuid = () => {
  const id = `00000000-0000-4000-8000-${String(state.allocations.length + 1).padStart(12, '0')}`;
  state.allocations.push(id);
  return id;
};
const context = vm.createContext({Mi: '<synthetic>', If: '(no content)', hmo: () => false, Kk: uuid});
const factory = vm.runInContext(`(()=>{${source.toString('utf8')};return Ys})()`, context);
const cases = [];
for (const [name, fields] of [
  ['generic API error', {}],
  ['refusal error category', {error: 'invalid_request'}],
  ['malformed-tool terminal subtype', {apiError: 'malformed_tool_use_exhausted'}],
]) {
  state.allocations = [];
  const row = factory({content: 'source', now: () => '2000-01-01T00:00:00.000Z', ...fields});
  assert.equal(state.allocations.length, 2);
  assert.equal(row.uuid, state.allocations[0]);
  assert.equal(row.message.id, state.allocations[1]);
  assert.notEqual(row.uuid, row.message.id);
  cases.push({name, fields, expected: {outer_uuid: row.uuid, inner_id: row.message.id,
    allocations: [...state.allocations], isApiErrorMessage: row.isApiErrorMessage}});
}
const output = {version: '2.1.319', native_sha256: nativeSha256,
  sourceByteRanges: [{name: 'Native Lpo/el/Ys assistant factories', start: base + begin,
    endExclusive: base + end, sha256: createHash('sha256').update(source).digest('hex')}],
  scope: 'Verbatim Native Ys/Lpo evaluation with a fresh deterministic UUID for every actual allocator call. Pins two independent allocations in order: outer uuid, then inner message.id. No provider, live CLI, or constant-UUID substitution is asserted.', cases};
const fixture = new URL('../../crates/orchestrator/tests/fixtures/synthetic_api_error_identity_2_1_319.json', import.meta.url);
const encoded = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(fixture, 'utf8'), encoded);
else fs.writeFileSync(fixture, encoded);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${cases.length} Native two-allocation observations`);
