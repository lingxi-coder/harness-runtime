#!/usr/bin/env node
// Controlled replay of exact, hash-pinned Claude Code 2.1.322 helpers.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
assert.equal(digest(binary), nativeSha256);
const ranges = [
  {
    "name": "normalize",
    "start": 260530248,
    "end": 260530385,
    "sha256": "2a939978d12b8f9007235f0b22462fde69be38a1ec2b1016ef30aeb03ece18c4"
  },
  {
    "name": "limits",
    "start": 266155065,
    "end": 266156167,
    "sha256": "0a769d0d577c02d03fdd10e1b14405cbd3bb7302329f9ea3eca7b4e96cfa3811"
  },
  {
    "name": "descriptionLimit",
    "start": 275068916,
    "end": 275068995,
    "sha256": "74a6e5248c6a04e615aff89fab51eae88915e287a25f292f9fa091e2a831bd68"
  },
  {
    "name": "utf16Cut",
    "start": 260461603,
    "end": 260461746,
    "sha256": "aba65f0627f912d07129e66e5cfbf35b189cd4ce73b0edc7f490e1f6d72a5b8a"
  },
  {
    "name": "utf16Copy",
    "start": 260462017,
    "end": 260462323,
    "sha256": "bfb3e4e6e725909a3e13582d6543d1621c15793b92a29d303029b85ef1c5ed94"
  },
  {
    "name": "truncate",
    "start": 305510004,
    "end": 305510157,
    "sha256": "040ac030475b449a12e6dd9ff55e5d80e380507e81024cf9f3d03865fc6ad6dc"
  },
  {
    "name": "provenance",
    "start": 267958047,
    "end": 267958290,
    "sha256": "d3b7bbc12d30ab386e668cd8398fb1d4462b517cba2877330e71ada3165dd3b4"
  }
];
const source = {};
for (const range of ranges) {
  const bytes = binary.subarray(range.start, range.end);
  assert.equal(digest(bytes), range.sha256, range.name);
  source[range.name] = bytes.toString('utf8');
}
let envLimit, ownedByMod = false;
const env = {};
Object.defineProperty(env, 'CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH', { get: () => envLimit });
const native = vm.runInNewContext(
  ranges.map(range => source[range.name]).join('\n') + '\n({normalize:Sn,limit:MB,truncate:wo,provenance:QIr});',
  { Buffer, i: env, QU: { ownerOfKey: () => ownedByMod ? 'fixture-mod' : undefined } },
  { timeout: 1000 },
);
const units = text => Array.from({ length: text.length }, (_, index) => text.charCodeAt(index));
const normalizationCases = [
  '', 'filesystem', 'a..b', 'A B', 'a😀b', '文档', 'claude.ai .a..b ',
  'claude.ai 😀', 'CLAUDE.AI 😀', '__prefix__', 'a/b:c@d', 'é', 'e\u0301',
].map(input => ({ input, expected: native.normalize(input) }));
const provenanceCases = [];
for (const scope of [undefined, 'dynamic', 'local', 'user', 'project', 'managed', 'enterprise', 'claudeai', 'agent']) {
  for (const serverType of ['stdio', 'sdk']) {
    for (const plugin of [false, true]) {
      for (const mod of [false, true]) {
        const input = { serverName: 'Raw.server 名', ...(scope === undefined ? {} : { scope }), serverType,
          ...(plugin ? { pluginTelemetry: {} } : {}), ownedByMod: mod };
        ownedByMod = mod;
        provenanceCases.push({ input, expected: JSON.parse(JSON.stringify(native.provenance(input))) });
      }
    }
  }
}
for (const value of ['', 'builtin', 'remote', 'plugin']) {
  const input = { serverName: 'explicit', scope: 'project', serverType: 'sdk', pluginTelemetry: {}, source: value };
  provenanceCases.push({ input, expected: JSON.parse(JSON.stringify(native.provenance(input))) });
}
const descriptionInputs = [
  { text: 'a', repeat: 4096, deferred: false },
  { text: 'a', repeat: 4097, deferred: false },
  { text: 'a', repeat: 4097, deferred: true },
  { text: 'a', repeat: 16384, deferred: true },
  { text: 'a', repeat: 16385, deferred: true },
  ...[false, true].map(deferred => ({ text: 'a', repeat: 20001, deferred, envLimit: 20000 })),
  ...['a😀b', '😀', 'e\u0301好', 'a\ud800b', '\udc00xy', 'a\ud800\ud800b'].flatMap(text =>
    [1, 2, 3, 4].map(envLimit => ({ text, repeat: 1, deferred: false, envLimit }))),
];
const descriptionCases = descriptionInputs.map(input => {
  envLimit = input.envLimit;
  const limit = native.limit(input.deferred);
  const text = input.text.repeat(input.repeat);
  const output = native.truncate(text, 'description', undefined, limit);
  // Store code units so Rust serde does not reject intentionally unpaired
  // input/output surrogates. expectedJson also locks native JSON bytes.
  return { inputUnits: units(input.text), repeat: input.repeat,
    deferred: input.deferred, envLimit: input.envLimit,
    expectedLimit: limit, expectedJson: JSON.stringify(output) };
});
const fixture = { version: '2.1.322', nativeSha256, ranges,
  scope: 'Exact pure normalization, ordinary/deferred description budgets and UTF-16 truncation; provenance folding with explicit host owner-registry seam. No provider, network, OAuth, or MCP lifecycle execution.',
  normalizationCases, provenanceCases, descriptionCases };
const path = new URL('../../crates/mcp/tests/fixtures/mcp_2_1_322.json', import.meta.url);
const rendered = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--write')) writeFileSync(path, rendered);
else assert.equal(readFileSync(path, 'utf8'), rendered, 'MCP 2.1.322 oracle drift');
console.log(`MCP 2.1.322: ${normalizationCases.length} naming, ${provenanceCases.length} provenance, ${descriptionCases.length} description cases match ${nativeSha256}`);
