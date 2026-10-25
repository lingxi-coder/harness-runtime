// Replay bounded, hash-pinned helpers from the official macOS arm64 artifact.
// Never starts Claude, imports its modules, or uses credentials/network/native APIs.
// Usage: node scripts/tests/computer_use_310_oracle.mjs /path/to/claude [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

const path = process.argv.slice(2).find((arg) => !arg.startsWith('--'));
if (!path) throw new Error('Supply the official Claude Code 2.1.310 macOS arm64 binary');
const binary = readFileSync(path);
const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');
const binarySha256 = digest(binary);
assert.equal(binarySha256, 'ffb64331bc0ee1b69efb7c2c428bd3ab516bc31b75bf60886b356c97c1c25ffd');
const ranges = {
  policy: [237497665, 237507683, 'd33f09a016e7163892b7e7fba1733c6747b565c6d2d50d540a609fdc540e0c01'],
  keys: [237507784, 237509805, '07cc1017e36c7e3370beb748fdbe5c3a4f1cbb9bef284790edb0c999af4acff0'],
  merge: [237624637, 237624861, '9698799ee4d00ac8ef26d69ab9f644f369cc428ffef5e4e4a8dd3c9f98e80468'],
  flags: [237459715, 237461149, '64ab8b197c16e2cf7e99e8f8bb01a50b115e8718b791d6a6c0b7cb9fd083af68'],
  scale: [237450200, 237451433, '24b0fb868f59874cb6f6cf34519dd6f910da8656cd0ebde05ece5c6725888146'],
  schema: [237461842, 237496484, '94d6ed18a4b71781b69e091a63c79b4e6c1bf7bdab168d2c641fae5ca4f91f99'],
  browserStatus: [262152565, 262153177, '339432bc1ee51ae90fe5af895b369996fd08c4f57f39f6a62abf44e539dff201'],
};
function source(name) {
  const [start, end, sha256] = ranges[name];
  const bytes = binary.subarray(start, end);
  assert.equal(digest(bytes), sha256, `Source range changed: ${name}`);
  return bytes.toString('utf8');
}
// Only the reviewed pure helper ranges run. No require, process, filesystem,
// network, native bindings or application bootstrap is exposed to the context.
function isolated(names, dependencies = {}) {
  const instance = vm.createContext(dependencies, { codeGeneration: { strings: false, wasm: false } });
  vm.runInContext(names.map(source).join('\n'), instance, { timeout: 1000 });
  return instance;
}
const contexts = Object.fromEntries(['flags', 'policy', 'keys', 'scale'].map((name) => [name, isolated([name])]));
contexts.merge = isolated(['merge'], { Hte: vm.runInContext('Hte', contexts.flags) });
contexts.schema = isolated(['schema'], vm.runInContext('({R0t,opt,_Cr})', contexts.scale));
// Bundled modules reuse minified identifiers. Keep their lexical scopes separate.
let context = contexts.policy;
const plain = (value) => JSON.parse(JSON.stringify(value));
function replay(expression) {
  return plain(vm.runInContext(expression, context, { timeout: 1000 }));
}
const fixture = {
  version: '2.1.310', platform: 'darwin-arm64', binarySha256,
  sourceByteRanges: Object.entries(ranges).map(([name, [start, end, sha256]]) => ({ name, start, end, sha256 })),
  classification: [], normalization: [], lookup: [], keys: [], sentinel: [], merge: [], schemas: [], browserStatus: [],
};
// Every identifier and fallback keyword in the actual classifier, plus precedence
// and case-sensitive identifier / case-insensitive display-name probes.
const identifiers = replay('[...new Set([...vn,...Tn,...In,...Ye,...Pn,...$n,...Dn,...Mn,...Nn,...Ln,...En])]');
const names = replay('[...new Set([...Fn,...Bn,...Un,...Rn])]');
const samples = [
  ...identifiers.map((bundleId) => ({ bundleId, displayName: 'Unclassified app' })),
  ...names.flatMap((name) => [name, name.toUpperCase(), `Prefix ${name} Suffix`].map((displayName) => ({ bundleId: 'org.example.unknown', displayName }))),
  { bundleId: 'com.google.Chrome', displayName: 'Terminal' },
  { bundleId: 'com.microsoft.VSCode', displayName: 'Chrome' },
  { bundleId: 'com.apple.finder', displayName: 'Finder' },
  { bundleId: 'COM.GOOGLE.CHROME', displayName: 'Unclassified app' },
  { bundleId: 'C:\\Program Files\\Google\\Chrome\\Application\\CHROME.EXE', displayName: 'Chrome' },
  { bundleId: '<unresolved>', displayName: 'Unknown' },
  { bundleId: undefined, displayName: 'Unknown' },
  { bundleId: undefined, displayName: 'Xcode TradingView Chrome' },
  { bundleId: undefined, displayName: 'Playbooks editor' },
  { bundleId: undefined, displayName: 'Play Books' },
];
for (const input of samples) {
  context.input = input;
  fixture.classification.push({ input, output: replay('({category:Q(input.bundleId,input.displayName),tier:ye(input.bundleId,input.displayName),policyDenied:K(input.bundleId,input.displayName)})') });
}
for (const input of samples.slice(0, identifiers.length)) {
  for (const tier of [undefined, 'read', 'click', 'full']) {
    const apps = [{ ...input, ...(tier ? { tier } : {}), grantedAt: 17 }];
    context.apps = apps;
    fixture.normalization.push({ input: { apps, denied: [], options: {} }, output: replay('Ve(apps,[])') });
  }
}
const mixedApps = [
  { bundleId: 'com.google.Chrome', displayName: 'Chrome', tier: 'full', grantedAt: 17 },
  { bundleId: 'com.microsoft.VSCode', displayName: 'VS Code', tier: 'full', grantedAt: 17 },
  { bundleId: 'com.apple.finder', displayName: 'Finder', tier: 'full', grantedAt: 17 },
  { bundleId: 'com.spotify.client', displayName: 'Spotify', tier: 'full', grantedAt: 17 },
];
for (const denied of [[], ['com.apple.finder'], ['com.google.Chrome', 'com.spotify.client']]) {
  for (const options of [{}, { forceFullTier: true }]) {
    context.input = { apps: mixedApps, denied, options };
    fixture.normalization.push({ input: plain(context.input), output: replay('Ve(input.apps,input.denied,input.options)') });
  }
}
for (const input of [
  { bundleId: 'com.google.Chrome', displayName: 'Chrome' },
  { bundleId: 'com.apple.finder', displayName: 'Finder' },
  { bundleId: 'com.spotify.client', displayName: 'Spotify' },
  { bundleId: 'org.example.unknown', displayName: 'Unknown' },
  { bundleId: undefined, displayName: 'Spotify' },
]) {
  context.input = input;
  context.apps = mixedApps;
  fixture.lookup.push({ input, output: replay('new be(apps,["com.apple.finder"]).lookup(input.bundleId,input.displayName)') });
}
context = contexts.keys;
for (const chord of [
  'cmd+c', 'ctrl+c', 'cmd+v', 'ctrl+v', 'CMD+X', 'shift+insert', 'ctrl+insert', 'shift+delete',
  'cmd+q', 'shift+cmd+q', 'ctrl+cmd+q', 'cmd+tab', 'cmd+space', 'alt+f4', 'win+l',
  'ctrl+alt+delete', 'shift+alt+tab', 'cmd+a', 'ctrl+shift+c', 'meta+c+v', 'q', '',
]) {
  context.chord = chord;
  fixture.keys.push({ input: chord, output: replay('({clipboard:[...yt(chord)],darwinSystem:_e(chord,"darwin"),win32System:_e(chord,"win32")})') });
}
context = contexts.flags;
for (const bundleId of replay('[...r,...s,...t,...d,...l,...p,"Microsoft.WindowsTerminal_abc!App","windows.immersivecontrolpanel_test!App","C:\\\\Windows\\\\System32\\\\CMD.EXE","org.example.app"]')) {
  context.bundleId = bundleId;
  fixture.sentinel.push({ input: bundleId, output: replay('({sentinel:men(bundleId),sensitive:pRs(bundleId)})') });
}
context = contexts.merge;
for (const input of [
  { apps: [], flags: {}, response: { granted: [], flags: {} } },
  { apps: mixedApps.slice(0, 1), flags: { clipboardRead: true }, response: { granted: [{ ...mixedApps[0], tier: 'read' }, mixedApps[2]], flags: { clipboardRead: false, clipboardWrite: true } } },
  { apps: [], flags: {}, response: { granted: [mixedApps[0], mixedApps[0]], flags: { clipboardRead: 1, clipboardWrite: 'true', systemKeyCombos: true } } },
]) {
  context.input = input;
  fixture.merge.push({ input, output: replay('_n(input.apps,input.flags,input.response)') });
}
// Store exact JSON byte hashes, not the reference's source or long descriptions.
// The structural input schema is also captured with descriptions removed.
function structure(value) {
  if (Array.isArray(value)) return value.map(structure);
  if (!value || typeof value !== 'object') return value;
  return Object.fromEntries(Object.entries(value).filter(([key]) => key !== 'description').map(([key, item]) => [key, structure(item)]));
}
context = contexts.schema;
for (const capabilities of [
  { platform: 'darwin', screenshotFiltering: 'native', appScoped: false },
  { platform: 'darwin', screenshotFiltering: 'native', appScoped: true },
  { platform: 'win32', screenshotFiltering: 'mask', appScoped: false },
  { platform: 'darwin', screenshotFiltering: 'none', adaptiveResolution: false, saveToDisk: false, appScoped: false },
]) {
  context.capabilities = capabilities;
  const tools = replay('rpt(capabilities,"pixels",[])');
  fixture.schemas.push({
    capabilities,
    tools: tools.map((tool) => ({ name: tool.name, jsonSha256: digest(JSON.stringify(tool)), inputSchema: structure(tool.inputSchema) })),
  });
}
for (const enabled of [undefined, false, true]) {
  for (const installed of [false, true, 'error']) {
    for (const connection of ['none', 'connecting', 'connected']) {
      const input = { enabled, installed, connection };
      const trace = [];
      const browserContext = isolated(['browserStatus'], {
        BB: 'controlled:install', M0e: 'controlled:reconnect', Va: 'claude-in-chrome',
        jB: async () => { if (installed === 'error') throw new Error('controlled detection failure'); return installed; },
        t: (message, options) => trace.push({ message, options }),
        ue: () => ({ ...(enabled !== undefined ? { claudeInChromeDefaultEnabled: enabled } : {}), chromeExtension: { pairedDeviceName: 'Controlled browser' } }),
        VA: () => true, vt: () => true, i: { isWslEnvironment: () => false },
        probe: { mcpClients: connection === 'none' ? [] : [{ name: 'claude-in-chrome', type: connection }], atStartup: async (value) => { trace.push({ atStartup: value }); return value; } },
      });
      const status = await vm.runInContext('Cki(probe)', browserContext, { timeout: 1000 });
      fixture.browserStatus.push({ input, output: plain(status), trace });
    }
  }
}
const output = JSON.stringify(fixture, null, 2) + '\n';
const destination = new URL('../../crates/tools/computer-use/fixtures/claude-code-2.1.310.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(destination, 'utf8'), output, 'Oracle fixture bytes differ');
else writeFileSync(destination, output);
console.log(JSON.stringify({ status: 'passed', binarySha256, fixtureSha256: digest(output), cases: Object.fromEntries(Object.entries(fixture).filter(([, value]) => Array.isArray(value)).map(([key, value]) => [key, value.length])) }));
