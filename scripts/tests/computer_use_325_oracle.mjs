// Replay reviewed, exact-byte helpers from the official Claude Code 2.1.325
// darwin-arm64 artifact. Never starts Claude or imports its bundled modules.
// Usage: node scripts/tests/computer_use_325_oracle.mjs /path/to/claude [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

const path = process.argv.slice(2).find(arg => !arg.startsWith('--'));
if (!path) throw new Error('Supply the official Claude Code 2.1.325 darwin-arm64 binary');
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const binary = readFileSync(path);
const binarySha256 = digest(binary);
assert.equal(binarySha256, '84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2');
const ranges = {
  policy: [302139122, 302148136, 'a0c179432fef302deba930afe845d37f5a169dca6404b113d839a0be119c8f76'],
  keys: [302088399, 302090484, '057ef814a6a51cdc2f7874207b2d4804cda25272a471139c44a99da3ccf2031c'],
};
function isolated(name) {
  const [start, end, sha256] = ranges[name];
  const bytes = binary.subarray(start, end);
  assert.equal(digest(bytes), sha256, `Source range changed: ${name}`);
  const context = vm.createContext({}, { codeGeneration: { strings: false, wasm: false } });
  vm.runInContext(bytes.toString('utf8'), context, { timeout: 1000 });
  return context;
}
const contexts = { policy: isolated('policy'), keys: isolated('keys') };
const plain = value => JSON.parse(JSON.stringify(value));
function replay(name, expression, input = undefined) {
  const context = contexts[name];
  // Construct inputs in the sandbox realm too: no host-object prototypes are
  // injected into the capability-free context.
  vm.runInContext(`globalThis.input=(${JSON.stringify(input) ?? 'undefined'})`, context, { timeout: 1000 });
  return plain(vm.runInContext(expression, context, { timeout: 1000 }));
}
const fixture = {
  version: '2.1.325', platform: 'darwin-arm64', binarySha256,
  scope: 'Pure app classification, grant normalization/lookup, and clipboard/system-key policy. Production differentials consume classification and keys only. No schema, native API, UI, browser or whole-protocol parity claim.',
  sourceByteRanges: Object.entries(ranges).map(([name, [start, end, sha256]]) => ({ name, start, end, sha256 })),
  classification: [], normalization: [], lookup: [], keys: [],
};
// Inventory every current policy identifier and fallback name from the reviewed
// source itself, then exercise identifier precedence, case and path handling.
const identifiers = replay('policy', '[...new Set([...gn,...wn,...yn,...Ge,...kn,...Cn,...An,...xn,...Sn,...vn,..._n])]');
const names = replay('policy', '[...new Set([...In,...Rn,...En,...bn])]');
const whitespace = [
  '\u0009', '\u000a', '\u000b', '\u000c', '\u000d', ' ', '\u00a0', '\u1680',
  '\u2000', '\u2001', '\u2002', '\u2003', '\u2004', '\u2005', '\u2006',
  '\u2007', '\u2008', '\u2009', '\u200a', '\u2028', '\u2029', '\u202f',
  '\u205f', '\u3000', '\ufeff', '\u0085', '\u180e', '\u200b',
];
const samples = [
  ...identifiers.map(bundleId => ({ bundleId, displayName: 'Unclassified app' })),
  ...names.flatMap(name => [name, name.toUpperCase(), `Prefix ${name} Suffix`].map(displayName => ({ bundleId: 'org.example.unknown', displayName }))),
  { bundleId: 'com.google.Chrome', displayName: 'Terminal' },
  { bundleId: 'com.microsoft.VSCode', displayName: 'Chrome' },
  { bundleId: 'com.apple.finder', displayName: 'Finder' },
  { bundleId: 'COM.GOOGLE.CHROME', displayName: 'Unclassified app' },
  { bundleId: 'C:\\Program Files\\Google\\Chrome\\Application\\CHROME.EXE', displayName: 'Chrome' },
  { bundleId: '<unresolved>', displayName: 'Unknown' },
  { displayName: 'Unknown' }, { displayName: 'Xcode TradingView Chrome' },
  { displayName: 'Playbooks editor' }, { displayName: 'Play Books' },
  ...whitespace.map(space => ({ bundleId: 'org.example.unknown', displayName: `Apple${space}TV` })),
];
for (const input of samples) {
  fixture.classification.push({ input, output: replay('policy', '({category:Q(input.bundleId,input.displayName),tier:ge(input.bundleId,input.displayName),policyDenied:z(input.bundleId,input.displayName)})', input) });
}
for (const input of samples.slice(0, identifiers.length)) {
  for (const tier of [undefined, 'read', 'click', 'full']) {
    const apps = [{ ...input, ...(tier ? { tier } : {}), grantedAt: 17 }];
    const parameters = { apps, denied: [], options: {} };
    fixture.normalization.push({ input: parameters, output: replay('policy', 'je(input.apps,input.denied,input.options)', parameters) });
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
    const input = { apps: mixedApps, denied, options };
    fixture.normalization.push({ input, output: replay('policy', 'je(input.apps,input.denied,input.options)', input) });
  }
}
for (const target of [
  { bundleId: 'com.google.Chrome', displayName: 'Chrome' },
  { bundleId: 'com.apple.finder', displayName: 'Finder' },
  { bundleId: 'com.spotify.client', displayName: 'Spotify' },
  { bundleId: 'org.example.unknown', displayName: 'Unknown' },
  { displayName: 'Spotify' },
]) {
  fixture.lookup.push({ input: target, output: replay('policy', 'new we(input.apps,["com.apple.finder"]).lookup(input.target.bundleId,input.target.displayName)', { apps: mixedApps, target }) });
}
const chords = new Set([
  'cmd+c', 'ctrl+c', 'cmd+v', 'ctrl+v', 'CMD+X', 'shift+insert', 'ctrl+insert', 'shift+delete',
  'cmd+q', 'shift+cmd+q', 'ctrl+cmd+q', 'cmd+tab', 'cmd+space', 'alt+f4', 'win+l',
  'ctrl+alt+delete', 'shift+alt+tab', 'cmd+a', 'ctrl+shift+c', 'meta+c+v', 'q', '',
  ...whitespace.flatMap(space => [`${space}cmd${space}+c`, `cmd+${space}v${space}`, `${space}shift+delete`, `${space}cmd+q`]),
]);
for (const modifier of ['meta', 'super', 'command', 'cmd', 'windows', 'win', 'ctrl', 'control', 'lctrl', 'lcontrol', 'rctrl', 'rcontrol', 'shift', 'lshift', 'rshift', 'alt', 'option']) {
  for (const key of ['c', 'x', 'v', 'insert', 'delete', 'q', 'tab', 'escape', 'esc']) {
    chords.add(`${modifier}+${key}`);
    chords.add(`${key}+${modifier}`);
  }
}
for (const input of chords) {
  fixture.keys.push({ input, output: replay('keys', '({clipboard:[...gUi(input)],darwinSystem:fen(input,"darwin"),win32System:fen(input,"win32")})', input) });
}
const output = JSON.stringify(fixture, null, 2) + '\n';
const destination = new URL('../../crates/tools/computer-use/fixtures/claude-code-2.1.325.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(destination, 'utf8'), output, 'Oracle fixture bytes differ');
else writeFileSync(destination, output);
console.log(JSON.stringify({ status: 'passed', binarySha256, fixtureSha256: digest(output), cases: Object.fromEntries(['classification', 'normalization', 'lookup', 'keys'].map(name => [name, fixture[name].length])) }));
