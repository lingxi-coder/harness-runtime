#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];
function take(first, last, offset, label) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < 3000, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha(bytes)});
  return bytes.toString();
}
function pin(source, offset, label) {
  const start = binary.indexOf(source, offset);
  assert(start >= offset && start - offset < 3000, label);
  const end = start + Buffer.byteLength(source);
  sourceByteRanges.push({label, start, end, sha256: sha(binary.subarray(start, end))});
  return source;
}
const integer = take('var N=/^[+-]?', 'function JL(', 179084600, 'actual integer environment coercer');
const boolean = take('function Le(e)', 'function zT(', 178927900, 'actual boolean environment coercers');
const schemas = take('function i(n)', 'export{Its,M};', 179192570, 'actual typed environment schemas');
const cap = pin('var Hu=2147483647;', 179629800, 'actual timer upper bound');
pin('sR=M.bool()', 179249400, 'remote environment uses the boolean schema');
const timeout = take('function uzt()', 'function fzt(', 188966000, 'actual nonstream timeout selector');
const validate = take('var of=(e,n)=>', 'var sf=', 185322680, 'actual SDK request integer validation');
pin('if("timeout"in s)of("timeout",s.timeout);', 185481400, 'SDK validates explicit timeout before building request');
pin('let b=uzt(),w=e.clock??qM', 188967300, 'nonstream drive captures its timeout once');
pin('timeout:b,...Object.keys(xe)', 188968000, 'selected timeout is passed to SDK request options');
const ctx = vm.createContext({process: {env: {}}, Un: class extends Error {}});
vm.runInContext('var $c=(()=>{' + integer + ';return $c})(),M=(()=>{' + boolean + schemas + ';return M})();' + cap + timeout + validate +
  'var a={get CLAUDE_CODE_REMOTE(){return M.bool().parse(remote)}};', ctx);
const apis = [null, '', '0', '-0', '+0', 'NaN', 'Infinity', '-Infinity', 'abc', 'undefined',
  '1', '-1', '+9', '1.5', '.5', '1ms', '1e3', '1e-3', '1.5e2', '1.5e0', '.5e3', '1.e2',
  '1e400', '-1e400', '1e3x', '0x10', '2147483647', '2147483648', '9007199254740993',
  '-9007199254740993', '1_000', '1,000', '1 000', '1\u00a0000', '1\u202f000',
  '1_000,000', '1,00', '1234,567', '-1_000', '+1_000', '12,345,678',
  '1' + '0'.repeat(400), '-1' + '0'.repeat(400),
  '1e' + '0'.repeat(29) + '3', '1e' + '0'.repeat(30) + '3'];
const whitespace = [' ', '\t', '\n', '\r', '\v', '\f', '\u00a0', '\u1680', '\u2000',
  '\u200a', '\u2028', '\u2029', '\u202f', '\u205f', '\u3000', '\ufeff', '\u0085', '\u180e', '\u001c'];
for (const space of whitespace) for (const value of ['1e3', '1_000', '-1', '0', 'abc']) {
  apis.push(space + value + space, space + value, value + space);
}
const remotes = [null, '', 'true', 'TRUE', '1', 'yes', 'on', 'false', '0', 'off', 'abc',
  ' true ', '\ufefftrue\ufeff', '\u0085true\u0085', '\u180etrue', 'truex'];
const encode = value => Number.isNaN(value) ? 'NaN' : value === Infinity ? 'Infinity' :
  value === -Infinity ? '-Infinity' : value;
const cases = [];
for (const api_timeout of apis) for (const remote of remotes) {
  ctx.process.env = api_timeout === null ? {} : {API_TIMEOUT_MS: api_timeout};
  ctx.remote = remote === null ? undefined : remote;
  const parsed = vm.runInContext('$c(process.env.API_TIMEOUT_MS)', ctx);
  let expected;
  try { expected = {millis: vm.runInContext('of("timeout",uzt())', ctx)}; }
  catch (error) { expected = {error: error.message}; }
  cases.push({api_timeout, remote, parsed: encode(parsed), expected});
}
const output = {
  version: '2.1.288', native_sha256: sha(binary), sourceByteRanges,
  scope: 'Actual integer and typed bool parsing, uzt timeout selection, SDK request integer validation and pinned caller wiring. Full CLI, long-stream retry limits and live-provider timing excluded.', cases,
};
const text = JSON.stringify(output, null, 2) + '\n';
const path = new URL('../../crates/llm-runtime/tests/fixtures/nonstream_timeout_2_1_288.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(path, 'utf8'), text);
else writeFileSync(path, text);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${cases.length} actual nonstream timeout cases`);
