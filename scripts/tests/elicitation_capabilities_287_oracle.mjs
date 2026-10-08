#!/usr/bin/env node
// Execute the exact elicitation capability helpers from the hash-pinned native
// reference. Only feature-service and host-environment dependencies are seams.
// Usage: node scripts/tests/elicitation_capabilities_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Pass the native Claude Code 2.1.287 binary as the first argument');
const binary = readFileSync(process.argv[2]);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const sha256 = digest(binary);
assert.equal(sha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');

function chunk(offset) {
  const marker = '// @bun @bytecode\n';
  assert.equal(binary.subarray(offset, offset + marker.length).toString('utf8'), marker);
  const end = binary.indexOf(marker, offset + marker.length);
  const bytes = binary.subarray(offset, end < 0 ? binary.length : end);
  const nul = bytes.indexOf(0, Math.max(bytes.lastIndexOf('export{'), 0));
  return bytes.subarray(0, nul < 0 ? bytes.length : nul);
}

const chunkOffset = 185762130;
const bytes = chunk(chunkOffset);
assert.equal(digest(bytes), 'cc43640e5f66aa04eb3757b878e50f5f5ecbf3fb35ac7a5d9cc7459276feb384');
const startMarker = 'function tnt(){';
const endMarker = 'function iHt(){';
const start = bytes.indexOf(startMarker);
const end = bytes.indexOf(endMarker, start + startMarker.length);
assert.equal(start, 452779);
assert.equal(end, 454077);
const selected = bytes.subarray(start, end);
const source = selected.toString('utf8');

let flags = {}, ccrProxy = false;
const helpers = vm.runInNewContext(`${source}\n({modern:Lsn,legacy:klt});`, {
  // k is the feature-service seam. A missing value uses the exact default the
  // native helper requests; present false/null/number/string values stay intact.
  k: (key, defaultValue) => Object.hasOwn(flags, key) ? flags[key] : defaultValue,
  IE: () => false,
  a: { CLAUDE_CODE_MCP_APPS_HOST: false },
  BP: () => 'v2',
  // CCR classification is supplied directly; no transport classifier is copied.
  ywe: () => ccrProxy,
}, { timeout: 2_000 });

const urlFlag = 'tengu_mcp_url_elicitation';
const legacyUrlFlag = 'tengu_mcp_legacy_url_elicitation';
const clone = value => JSON.parse(JSON.stringify(value));
function evaluate(input) {
  flags = input.flags;
  ccrProxy = input.ccr_proxy;
  const modern = helpers.modern();
  const legacy = helpers.legacy(input.server, { denylisted: input.denylisted });
  // IE/apps-host are false throughout: this also checks that the host seams do
  // not leak tasks/extensions into the capability outputs being projected.
  for (const capabilities of [modern, legacy]) {
    assert.deepEqual(clone(capabilities.roots), { listChanged: true });
    assert.equal(capabilities.tasks, undefined);
    assert.equal(capabilities.extensions, undefined);
  }
  return { modern_lsn: clone(modern.elicitation), legacy_klt: clone(legacy.elicitation) };
}

const transportTypes = [undefined, 'stdio', 'http', 'sse', 'ws', 'sse-ide', 'ws-ide', 'sdk', 'claudeai-proxy'];
const bareValues = [undefined, false, true];
const flagPairs = [
  { name: 'flags-default', values: {} },
  ...[false, true].flatMap(url => [false, true].map(legacy => ({
    name: `url-${url}-legacy-${legacy}`,
    values: { [urlFlag]: url, [legacyUrlFlag]: legacy },
  }))),
];
const cases = [];
for (const type of transportTypes) {
  for (const bare of bareValues) {
    for (const pair of flagPairs) {
      for (const ccr_proxy of [false, true]) {
        for (const denylisted of [false, true]) {
          const input = {
            server: {
              ...(type === undefined ? {} : { type }),
              ...(bare === undefined ? {} : { bareElicitationCapability: bare }),
            },
            flags: pair.values,
            ccr_proxy,
            denylisted,
          };
          cases.push({
            name: `${type ?? 'absent'}/bare-${bare ?? 'absent'}/${pair.name}/ccr-${ccr_proxy}/denylisted-${denylisted}`,
            input,
            expected: evaluate(input),
          });
        }
      }
    }
  }
}
assert.equal(cases.length, 540);

// Preserve the native distinction: tnt uses ordinary JavaScript truthiness,
// while nnt accepts only strict boolean true. These values are service outputs,
// not a replacement implementation of either helper.
const flagValueCases = [];
for (const [label, value] of [
  ['false', false], ['true', true], ['zero', 0], ['one', 1],
  ['string-false', 'false'], ['string-true', 'true'], ['null', null],
  ['object', {}], ['array', []],
]) {
  for (const key of [urlFlag, legacyUrlFlag]) {
    const input = { server: { type: 'http' }, flags: { [key]: value }, ccr_proxy: false, denylisted: false };
    flagValueCases.push({ name: `${key}/${label}`, input, expected: evaluate(input) });
  }
}

const fixture = {
  version: '2.1.287',
  sha256,
  scope: 'Actual native tnt/nnt/Txr/jAn/WAn/sHt/GAn/Lsn/klt functions execute unchanged. Feature service k, IE=false, apps-host=false, BP=v2 and CCR classifier ywe are explicit seams. Each case projects the elicitation object returned by modern Lsn and legacy klt. No connection, negotiation, retry, transport, process or network execution.',
  seams: { IE: false, CLAUDE_CODE_MCP_APPS_HOST: false, BP: 'v2', feature_service: 'case flags or native requested default', ywe: 'case ccr_proxy' },
  source_slices: [{
    source_chunk: 'src_185762130.js',
    source_chunk_offset: chunkOffset,
    source_chunk_sha256: digest(bytes),
    start_marker: startMarker,
    end_marker: endMarker,
    relative_start: start,
    relative_end: end,
    start: chunkOffset + start,
    end: chunkOffset + end,
    sha256: digest(selected),
    source,
  }],
  case_count: cases.length + flagValueCases.length,
  cases,
  flag_value_cases: flagValueCases,
};
assert.equal(fixture.case_count, 558);
assert.equal(new Set([...cases, ...flagValueCases].map(test => test.name)).size, fixture.case_count);
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const outputPath = fileURLToPath(new URL('../../crates/mcp/tests/fixtures/elicitation_capabilities_2_1_287.json', import.meta.url));
if (process.argv.includes('--check')) {
  const existing = readFileSync(outputPath, 'utf8');
  assert.deepEqual(JSON.parse(existing), fixture, 'Elicitation capability fixture values differ');
  assert.equal(existing, output, 'Elicitation capability fixture formatting differs');
  console.log(`Verified ${fixture.case_count} elicitation capability cases from Claude Code ${fixture.version} (${sha256})`);
} else {
  mkdirSync(dirname(outputPath), { recursive: true });
  writeFileSync(outputPath, output);
  console.log(`Wrote ${fixture.case_count} elicitation capability cases to ${outputPath}`);
}
