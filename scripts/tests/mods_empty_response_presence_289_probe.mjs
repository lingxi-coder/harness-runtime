#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const windows = [
  { label: 'Native no-content text constant', start: 181809988, end: 181810009, sha256: '6f46bfb26da2c3a986c65fcf061f27296adc857feb87ee2378a79695b10c2fc5' },
  { label: 'Native nonstream empty-content placeholder', start: 189342866, end: 189342954, sha256: '1a062842d3ce72f3b452d4e79637401f1fcc40432624dd15568f5dc209621980' },
];
const hash = value => createHash('sha256').update(value).digest('hex');
const nativeBinarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const args = process.argv.slice(2);
const binary = await readFile(args.find(arg => !arg.startsWith('--')) ?? '/tmp/harness-claude-code-2.1.289-darwin-arm64');
assert.equal(hash(binary), nativeBinarySha256);
const sources = windows.map(window => {
  const bytes = binary.subarray(window.start, window.end);
  assert.equal(hash(bytes), window.sha256, window.label);
  return bytes.toString('utf8');
});
const cases = [
  { name: 'removed raw control', raw: [{ type: 'fallback' }], accepted: [], expected: [{ type: 'text', text: '(no content)', citations: [] }] },
  { name: 'empty raw content', raw: [], accepted: [], expected: [] },
  { name: 'missing raw content', accepted: [], expected: [] },
  { name: 'existing accepted text', raw: [{ type: 'text', text: 'answer' }], accepted: [{ type: 'text', text: 'answer' }], expected: [{ type: 'text', text: 'answer' }] },
];
const output = cases.map(({ name, raw, accepted, expected }) => {
  const context = { Ji: structuredClone(accepted), Wn: { content: raw } };
  vm.runInNewContext(`${sources[0]};${sources[1]}`, context, { timeout: 5000 });
  const content = JSON.parse(JSON.stringify(context.Ji));
  assert.deepEqual(content, expected, name);
  return { name, content };
});
const report = {
  result: 'PASS', native_binary_sha256: nativeBinarySha256, source_windows: windows,
  cases: output,
  scope: 'Executes the exact Native nonstream placeholder statement and text constant on controlled pre/post-filter arrays; the content filtering, fallback admission, full CLI, network and provider are not executed.',
};
if (args.includes('--check')) {
  const fixture = JSON.parse(await readFile(new URL('../../docs/parity/mods-empty-response-presence-2.1.289.json', import.meta.url), 'utf8'));
  assert.deepEqual(report, fixture);
  process.stdout.write('Native empty-response citations presence fixture passed\n');
} else {
  process.stdout.write(JSON.stringify(report, null, 2) + '\n');
}
