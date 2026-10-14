#!/usr/bin/env node
// Source-pinned Native 2.1.289 Client constructor and surface VM probe.
// This runs the exact Client JSX helper and embedded surface runtime with a
// controlled h() and host bridge. It does not run Go's plugin loader, the
// Desktop renderer, or external frame IPC.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node scripts/tests/mods_ui_client_289_probe.mjs <2.1.289-binary>');
const sha256 = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(binaryPath);
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const pins = {
  clientFactory: {
    start: 182690001,
    end: 182691156,
    sha256: '2186ea7d75b18bf2f3bee33cf85526c4488ddeb2c0017c2b9d7fef3fffa2fc75',
    marker: '  function client(props, children) {',
  },
  clientValidator: {
    start: 186027762,
    end: 186028588,
    sha256: '5f36aab9810e3e5f33ad764891b2029541b3d99b10c8fda29ea6dacb26102d19',
    marker: 'function cBr(e){',
  },
  resolverIdentity: {
    start: 182871426,
    end: 182871916,
    sha256: '7f7b2c725f18b0c6365e387a606b7b89de008fb1094f7332442c263e9db995e0',
    marker: 'var Te=(e,r)=>({...e,client:{plugin:r}});',
  },
  surfaceModuleDiscovery: {
    start: 188332817,
    end: 188334058,
    sha256: 'b00b52f6aac7c2d493d1ef781287302cc7a593b22f7af307b158dcdfe1f7c8a0',
    marker: 'async function WEt(e,n,r){',
  },
  moduleLiteralScanner: {
    start: 182570297,
    end: 182570467,
    sha256: '2fb781e4db43dcb5d6ca88fdd0296e8f6dbc856008d62d4a06c7879a558d3402',
    marker: 'function bo(e){',
  },
  moduleRelativeGuard: {
    start: 182563597,
    end: 182563669,
    sha256: 'ffc36282ee4ef68f905efdbf702204fd5735f943e968624635ae62499ebb9b96',
    marker: 'var _5r=',
  },
  modulePathNormalization: {
    start: 182571549,
    end: 182571984,
    sha256: 'fe3b7949be319500ada3f0de551047f3d0080a87840286b803ec6ee0dfcd26d6',
    marker: 'function So(e){',
  },
  surfaceExportSelection: {
    start: 182571149,
    end: 182571461,
    sha256: 'dd7657efa37b1e3daee66d83e0a1c8224ecb295eef254f154e56136bb28fa81f',
    marker: 'function $as(e){',
  },
  normalizedClientModuleList: {
    start: 182572323,
    end: 182572608,
    sha256: '67d98b189290dbddbc2ebb67394e657378640025282349b0fef58d44d64c04f4',
    marker: 'function Or(e){',
  },
  surfaceRuntime: {
    start: 186080973,
    end: 186093793,
    sha256: 'fecdc1d1e8159a8552f15ef162b15a6c17722fb0a2c3f383efef47aef50f98d4',
    marker: 'var g8n=String.raw`',
  },
  desktopModuleLoader: {
    start: 197823671,
    end: 197827134,
    sha256: 'd861064afd254ecc64170b9e605048f9e746c515a78ccee9a2861bb91c2147da',
    marker: 'async function Go(r,i={}){',
  },
  desktopEnvironmentLifecycle: {
    start: 197827134,
    end: 197828466,
    sha256: '9fca23a3c5f5780551309f2eb4eb43cfbe744bb1a0d9420f4dce437065f38100',
    marker: 'var Le=',
  },
  desktopDrawBridge: {
    start: 197828478,
    end: 197835868,
    sha256: '3f6f220aa7176322cf61eff70e8db40567f62ddec80d0e9abb07cbc8ea96e8ad',
    marker: 'function zbr(r={})',
  },
  uiMessageEnvelope: {
    start: 186094985,
    end: 186095123,
    sha256: 'd2b82f3e56ff7008f3908d6d4367b199c7e29048f5dd51a7f1a34cb6b7b0ba4a',
    marker: 'var tgn=',
  },
  uiRenderModuleHashes: {
    start: 186098500,
    end: 186099268,
    sha256: 'bfe3b1722480f6203de10c29e765bf364284bbacb803d0439731d38c4f37ad8e',
    marker: 'a8({async run(e){',
  },
};

function source(name) {
  const pin = pins[name];
  const bytes = binary.subarray(pin.start, pin.end);
  assert.equal(bytes.length, pin.end - pin.start, `${name}: source length changed`);
  assert.equal(sha256(bytes), pin.sha256, `${name}: source hash changed`);
  assert(bytes.toString('utf8').startsWith(pin.marker), `${name}: source marker moved`);
  return bytes.toString('utf8');
}

const nativeClientFactory = source('clientFactory');
source('clientValidator');
source('resolverIdentity');
const nativeModuleLiteralScanner = source('moduleLiteralScanner');
const nativeRelativeGuard = source('moduleRelativeGuard');
const nativeModulePathNormalization = source('modulePathNormalization');
source('surfaceExportSelection');
source('normalizedClientModuleList');
source('surfaceModuleDiscovery');
const nativeSurfaceRuntime = source('surfaceRuntime');
source('desktopModuleLoader');
source('desktopEnvironmentLifecycle');
source('desktopDrawBridge');
source('uiMessageEnvelope');
source('uiRenderModuleHashes');

const client = vm.runInNewContext(`${nativeClientFactory}; client`);
const inputProps = {
  module: './board.tsx',
  key: 'board',
  props: {title: 'Status', rows: [1, 2]},
  width: '80%',
  height: 12,
  flexGrow: 2,
};
assert.deepEqual(JSON.parse(JSON.stringify(client(inputProps, []))), {
  type: 'Client',
  props: {
    key: 'board',
    module: './board.tsx',
    props: {title: 'Status', rows: [1, 2]},
    width: '80%',
    height: 12,
    flexGrow: 2,
  },
  client: {plugin: ''},
});
assert.equal('data' in client(inputProps, []), false,
  'JSX data is renamed to props.props; it is not an emitted data field');
assert.throws(() => client(inputProps, ['child']), /is a leaf/,
  'Native Client constructor rejects children');

// `So` receives the Client's literal in the importing source-file context,
// then normalizes it to a plugin-root-relative module key before `WEt` loads
// that file. The suffix predicate is a controlled stand-in for Native's
// imported source-extension list; the relative-path and root checks are exact.
assert.match(nativeModuleLiteralScanner, /br\(r\)/,
  'Native scanner visits helper calls to collect surface module literals');
const modulePathResolver = vm.runInNewContext(
  `${nativeRelativeGuard};${nativeModulePathNormalization};So`,
  {
    FQr: 'the supported source extensions',
    y5r: value => /\.(?:js|jsx|ts|tsx)$/.test(value),
    Ss: path.dirname,
    ws: path.isAbsolute,
    vr: path.resolve,
    vs: path.relative,
    Tr: path.sep,
  },
);
assert.deepEqual(
  JSON.parse(JSON.stringify(modulePathResolver({
    spelled: './board.tsx',
    file: '/plugins/example/hooks/register.ts',
    root: '/plugins/example',
  }))),
  {module: 'hooks/board.tsx'},
  'Native normalizes a source-relative literal to a plugin-relative key',
);
assert.match(modulePathResolver({
  spelled: 'file:///tmp/board.tsx',
  file: '/plugins/example/hooks/register.ts',
  root: '/plugins/example',
}).problem, /is not relative/,
'Native surface-module entries are not loaded from file or network URLs');
assert.match(modulePathResolver({
  spelled: '../../outside.tsx',
  file: '/plugins/example/hooks/register.ts',
  root: '/plugins/example',
}).problem, /leaves the plugin/,
'Native refuses a Client module path that resolves outside its plugin');

const runtimeMatch = nativeSurfaceRuntime.match(/^var g8n=String\.raw`([\s\S]*)`$/);
assert(runtimeMatch, 'expected exact embedded surface runtime template');
const calls = {scheduled: [], posted: [], startedTimers: [], stoppedTimers: [], faults: []};
let nextTimer = 1;
const host = {
  schedule: id => calls.scheduled.push(id),
  post: (id, text) => calls.posted.push({id, text}),
  startTimer: (id, ms) => {
    const timer = nextTimer++;
    calls.startedTimers.push({id, ms, timer});
    return timer;
  },
  stopTimer: (id, timer) => calls.stoppedTimers.push({id, timer}),
  fault: (id, reason) => calls.faults.push({id, reason}),
};
const limits = {nodes: 20_000, depth: 32, chars: 100_000, values: 20_000, dataDepth: 32};
const sandbox = vm.createContext({
  h: (type, props, ...children) => ({type, props, children}),
});
const factory = vm.runInContext(runtimeMatch[1], sandbox);
const runtime = factory(host, limits);
const clientRuntimeId = 7;
const seen = {pointer: [], key: [], ticks: 0};
let surfaceApi;
let cancelTimer;
let installedListeners = false;
const label = 'Native tree';
runtime.mount(clientRuntimeId, (props, surface) => {
  surfaceApi = surface;
  if (!installedListeners) {
    installedListeners = true;
    surface.onPointer(event => seen.pointer.push(event));
    surface.onKey(event => seen.key.push(event));
    cancelTimer = surface.every(15, () => { seen.ticks += 1; });
    surface.setState({ready: true});
    surface.post({kind: 'ready', label: props.label});
  }
  return surface.elements.Box({
    flexDirection: 'column',
    children: [surface.elements.Text({children: props.label})],
  });
}, {label});
runtime.resize(clientRuntimeId, 80, 24);
runtime.stage(clientRuntimeId, 'render');
const treeText = sandbox.__surface__.run();
assert.equal(typeof treeText, 'string');
assert.deepEqual(JSON.parse(treeText), {
  type: 'Box',
  props: {flexDirection: 'column'},
  children: [{type: 'Text', props: {}, children: [label]}],
});
assert.deepEqual(Object.keys(surfaceApi.elements).sort(),
  ['Box', 'Button', 'Code', 'Input', 'Link', 'Markdown', 'Select', 'Text']);
assert.deepEqual(calls.scheduled, [clientRuntimeId],
  'surface.setState asks the host to schedule a render');
assert.deepEqual(calls.posted, [{
  id: clientRuntimeId,
  text: JSON.stringify({kind: 'ready', label}),
}], 'surface.post sends JSON data through the host post callback');
assert.deepEqual(calls.startedTimers, [{id: clientRuntimeId, ms: 15, timer: 1}]);

const pointer = {kind: 'pointer-event', x: 4, y: 9};
const key = {kind: 'key-event', key: 'Enter'};
runtime.stage(clientRuntimeId, 'pointer', pointer);
sandbox.__surface__.run();
runtime.stage(clientRuntimeId, 'key', key);
sandbox.__surface__.run();
runtime.stage(clientRuntimeId, 'tick', 1);
sandbox.__surface__.run();
assert.deepEqual(seen.pointer, [pointer], 'surface pointer listener receives its host payload unchanged');
assert.deepEqual(seen.key, [key], 'surface key listener receives its host payload unchanged');
assert.equal(seen.ticks, 1, 'host tick invokes its registered surface callback');
cancelTimer();
assert.deepEqual(calls.stoppedTimers, [{id: clientRuntimeId, timer: 1}]);

// Execute the nested tree in its own vm global, sharing the same exact runtime source.
const nestedSandbox = vm.createContext({h: (type, props, ...children) => ({type, props, children})});
const nestedFactory = vm.runInContext(runtimeMatch[1], nestedSandbox);
const nestedHost = {...host};
const nestedSurfaceRuntime = nestedFactory(nestedHost, limits);
nestedSurfaceRuntime.mount(8, (_props, surface) => ({
  type: 'Client', props: {key: 'nested', module: './nested.tsx'},
}), {});
nestedSurfaceRuntime.stage(8, 'render');
assert.throws(() => nestedSandbox.__surface__.run(), /a Client inside the tree/,
  'surface modules cannot return nested Client nodes');

console.log('passed 2.1.289 Client constructor/path normalization, surface JSON tree, host callbacks, and nested-Client refusal (controlled extension predicate/h/host; Go/desktop renderer not executed)');
