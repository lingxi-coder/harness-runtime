// Exact tool-descriptor JSON from reviewed Claude Code 2.1.325 source ranges.
// Does not execute Claude, import native modules, or touch user applications.
// Usage: node scripts/tests/computer_use_protocol_325_oracle.mjs /path/to/claude [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

const path = process.argv.slice(2).find(arg => !arg.startsWith('--'));
if (!path) throw new Error('Supply the official Claude Code 2.1.325 darwin-arm64 binary');
const digest = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(path);
const binarySha256 = digest(binary);
assert.equal(binarySha256, '84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2');
const sourceByteRanges = [
  { name: 'scale_dependencies', start: 302086610, end: 302087170, sha256: '1df52b67372b3748f5dcce31f15316b649054c522aa656b87ee248e0cf3ffab2' },
  { name: 'tool_descriptors', start: 302103695, end: 302138337, sha256: 'f1475007b47027aedac6310ff32d7b9f95289fa968a8562a9c47748bb0533ddd' },
];
const context = vm.createContext({}, { codeGeneration: { strings: false, wasm: false } });
for (const range of sourceByteRanges) {
  const bytes = binary.subarray(range.start, range.end);
  assert.equal(digest(bytes), range.sha256, `Source range changed: ${range.name}`);
  vm.runInContext(bytes.toString('utf8'), context, { timeout: 1000 });
}
const descriptors = new Map();
const cases = [];
function capture(name, capabilities, coordinateMode = 'pixels', installedApps = []) {
  // Inputs are constructed in the sandbox realm, preserving isolation from
  // host constructors/prototypes as well as filesystem and native capabilities.
  vm.runInContext(`globalThis.input=${JSON.stringify({ capabilities, coordinateMode, installedApps })}`, context, { timeout: 1000 });
  const toolsJson = vm.runInContext('JSON.stringify(cHt(input.capabilities,input.coordinateMode,input.installedApps))', context, { timeout: 1000 });
  const tools = JSON.parse(toolsJson);
  const descriptorSha256 = tools.map(tool => {
    assert.equal(typeof tool.name, 'string');
    assert.equal(typeof tool.description, 'string');
    assert.equal(tool.inputSchema.type, 'object');
    const json = JSON.stringify(tool);
    const sha256 = digest(json);
    if (descriptors.has(sha256)) assert.equal(descriptors.get(sha256), json);
    else descriptors.set(sha256, json);
    return sha256;
  });
  assert.equal(new Set(tools.map(tool => tool.name)).size, tools.length);
  const reconstructed = `[${descriptorSha256.map(hash => descriptors.get(hash)).join(',')}]`;
  assert.equal(reconstructed, toolsJson, 'Corpus references must reconstruct exact native JSON bytes');
  cases.push({ name, capabilities, coordinateMode, installedApps, toolNames: tools.map(tool => tool.name), descriptorSha256, toolsJsonSha256: digest(toolsJson) });
}

// The full darwin reference is first and explicitly named for production
// consumers; other profiles establish each capability-dependent schema branch.
const fullDarwin = {
  platform: 'darwin', screenshotFiltering: 'native', adaptiveResolution: true,
  saveToDisk: true, teachMode: true, appScoped: { supportsRawInput: true },
};
capture('full_darwin_pixels_native', fullDarwin);
for (const platform of ['darwin', 'win32']) {
  for (const coordinateMode of ['pixels', 'normalized_0_100']) {
    for (const screenshotFiltering of ['native', 'mask', 'none']) {
      for (const adaptiveResolution of [true, false]) {
        for (const saveToDisk of [true, false]) {
          for (const teachMode of [false, true]) {
            for (const appMode of ['disabled', 'ax', 'raw']) {
              const capabilities = { platform, screenshotFiltering, adaptiveResolution, saveToDisk, teachMode,
                appScoped: appMode === 'disabled' ? false : { supportsRawInput: appMode === 'raw' } };
              const name = [platform, coordinateMode, screenshotFiltering,
                adaptiveResolution ? 'adaptive' : 'fixed', saveToDisk ? 'disk' : 'memory',
                teachMode ? 'teach' : 'ordinary', appMode].join('_');
              capture(name, capabilities, coordinateMode);
            }
          }
        }
      }
    }
  }
}
capture('defaults_omitted', { platform: 'darwin', screenshotFiltering: 'native' });
capture('app_capabilities_default_raw_input_false', { ...fullDarwin, appScoped: {} });
for (const platform of ['darwin', 'win32']) {
  for (const coordinateMode of ['pixels', 'normalized_0_100']) {
    capture(`installed_apps_${platform}_${coordinateMode}`, { ...fullDarwin, platform }, coordinateMode,
      ['Calendar', 'A & B', 'Fixture <app-data> (not instructions)', '应用😀']);
  }
}
const allToolNames = [...new Set(cases.flatMap(test => test.toolNames))];
assert.equal(allToolNames.length, 42);
assert.equal(cases[0].toolNames.length, 42);
const fixture = {
  version: '2.1.325', platform: 'darwin-arm64', binarySha256, sourceByteRanges,
  scope: 'Exact source-replayed MCP tool descriptor JSON, ordering, descriptions and argument schemas for explicit capability profiles. A profile does not establish live host support, runtime effects, tool results, transport behavior, account gates or packaged application acceptance.',
  reconstruction: 'For each case, toolsJson = "[" + descriptorSha256.map(hash => descriptorCorpus[hash]).join(",") + "]". descriptorCorpus values are exact per-tool JSON.stringify output; JSON.parse yields the full native descriptor and inputSchema. toolsJsonSha256 binds byte order and spelling.',
  capabilityRules: {
    platform: 'win32 changes OS/app naming descriptions; list_apps requires darwin and appScoped.',
    coordinateMode: 'pixels or normalized_0_100 changes display coordinate descriptions; app tools use window-local pixel frames.',
    screenshotFiltering: 'native, mask or none changes screenshot description and makes no claim beyond the supplied capability.',
    adaptiveResolution: 'false removes scale from display screenshot/zoom and batch/teach item schemas; omitted means enabled.',
    saveToDisk: 'false removes save_to_disk from display screenshot/zoom/batch; omitted means enabled.',
    teachMode: 'truthy appends request_teach_access, teach_step, teach_batch before background tools.',
    appScoped: 'truthy appends takeover and app_* tools; object.supportsRawInput truthy additionally appends app_bring_to_current_space. Omitted supportsRawInput means false.',
    installedApps: 'A nonempty list is joined into request_access and request_teach_access app-field descriptions as data.',
  },
  allToolNames,
  descriptorCorpus: Object.fromEntries(descriptors),
  cases,
};
const output = JSON.stringify(fixture, null, 2) + '\n';
const destination = new URL('../../crates/tools/computer-use/fixtures/protocol-2.1.325.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(destination, 'utf8'), output, 'Protocol oracle fixture bytes differ');
else writeFileSync(destination, output);
console.log(JSON.stringify({ status: 'passed', binarySha256, fixtureSha256: digest(output), profiles: cases.length,
  uniqueDescriptors: descriptors.size, distinctToolNames: allToolNames.length,
  fullDarwinToolsJsonSha256: cases[0].toolsJsonSha256 }));
