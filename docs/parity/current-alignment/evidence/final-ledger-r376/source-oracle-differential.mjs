import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Script } from 'node:vm';
import { ClientSurfaceManager } from 'file:///Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/crates/hooks/src/mod_ui_client_worker.mjs';

const sourcePath = '/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/acceptance/source-alignment-295-r31/skill-display-name-source/src_192995347.js';
const source = readFileSync(sourcePath, 'utf8');
const sourceSha256 = createHash('sha256').update(source).digest('hex');
assert.equal(sourceSha256, 'cfaaecaee8d26184f098f50f71d96e032ee78508ff5e67f8e8e7a979344223a3');
const start = source.indexOf('  function exportTree(instance, tree) {');
const end = source.indexOf('\n  // What a rejection says', start);
assert(start >= 0 && end > start);
const nativeExportTree = new Function(
  '_isFinite', '_isArray', '_keys', 'defineProperty', '_stringify',
  'limits', 'PRESSABLE', 'fail',
  `${source.slice(start, end)}; return exportTree;`,
)(
  Number.isFinite,
  Array.isArray,
  Object.keys,
  Object.defineProperty,
  JSON.stringify,
  { nodes: 20_000, values: 20_000, dataDepth: 32, depth: 32, chars: 100_000 },
  { Button: 'onPress', Input: 'onEvent', Select: 'onEvent' },
  (message, Ctor = Error) => { throw new Ctor(message); },
);

const srcDir = '/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/crates/hooks/src';
const helperSource = readFileSync(join(srcDir, 'mod_ui_client_helpers.mjs'), 'utf8');
const hookTypesSource = readFileSync(join(srcDir, 'mod_ui_hooks_types.mjs'), 'utf8');
const boardSource = `export function Board(_props, surface) { return surface.elements.Text({ children: 'ready' }); }`;
const manager = new ClientSurfaceManager({ send() {} });
const mounted = await manager.mount({
  plugin: 'oracle', environmentId: 'copyjson-r352', runtimeId: 'copyjson-r352',
  parent: { surface: 'desktop', component: 'panel', requestId: 'oracle-1' },
  client: { key: 'board', module: 'board.mjs' },
  moduleGraph: {
    modules: [{ module: 'board.mjs', component: 'Board', source: boardSource }],
    manifest: {
      runtime: 'claude:surface-runtime',
      modules: [{ module: 'board.mjs', entry: 'board.mjs', component: 'Board' }],
      files: [
        { key: 'claude:surface-runtime', source: helperSource },
        { key: 'claude:hooks-types', source: hookTypesSource },
        { key: 'board.mjs', source: boardSource },
      ],
    },
  },
  props: {}, columns: 80, rows: 24,
});
const environment = manager.runtimeInstances.get(mounted.runtimeId).environment;
const invoke = new Script('(serializeTree, tree, bridge) => serializeTree(tree, bridge)')
  .runInContext(environment.context);
function hSerialize(tree) {
  let next = 0;
  return invoke(environment.serializeTree, tree, { hold() { next += 1; return next; } });
}
function nativeSerialize(tree) {
  return nativeExportTree({ heldCounter: 0, held: new Map() }, tree);
}
const results = [];
function pair(label, nativeTree, hTree = nativeTree) {
  const expected = nativeSerialize(nativeTree);
  const actual = hSerialize(hTree);
  assert.equal(actual, expected, `${label}: raw JSON bytes differ`);
  results.push({ case: label, nativeRaw: expected, hRaw: actual });
  console.log(`PASS ${label} (${actual.length} UTF-16 code units)`);
}
function pairedFailure(label, nativeTree, hTree = nativeTree) {
  let nativeError;
  let hError;
  try { nativeSerialize(nativeTree); } catch (error) { nativeError = error; }
  try { hSerialize(hTree); } catch (error) { hError = error; }
  assert(nativeError, `${label}: Native source unexpectedly accepted input`);
  assert(hError, `${label}: H unexpectedly accepted input`);
  assert.equal(hError.message, nativeError.message, `${label}: direct exporter error text differs`);
  results.push({ case: label, nativeError: nativeError.message, hError: hError.message });
  console.log(`PASS ${label} (source-level rejection: ${nativeError.message})`);
}

pair('root-string', 'root text');

function customDataTree() {
  const values = [undefined, () => {}, 1n, Symbol('array'), 'kept'];
  values.extra = 'ignored';
  values[Symbol('array-key')] = 'ignored';
  const inheritedValues = new Array(1);
  const valuesPrototype = Object.create(Array.prototype);
  Object.defineProperty(valuesPrototype, '0', { value: 'inherited', enumerable: true });
  Object.setPrototypeOf(inheritedValues, valuesPrototype);
  inheritedValues.extra = 'ignored';
  const props = Object.create({ inherited: 'ignored' });
  Object.defineProperties(props, {
    visible: { enumerable: true, value: 7 },
    read: { enumerable: true, get() { return 'got'; } },
    bigint: { enumerable: true, value: 1n },
    symbol: { enumerable: true, value: Symbol('object') },
    callback: { enumerable: true, value() {} },
    values: { enumerable: true, value: values },
    inheritedValues: { enumerable: true, value: inheritedValues },
    nullable: { enumerable: true, value: null },
    missing: { enumerable: true, value: undefined },
    nonfinite: { enumerable: true, value: Infinity },
    toJSON: { enumerable: true, value() { throw new Error('toJSON must not run'); } },
    hidden: { enumerable: false, get() { throw new Error('non-enumerable getter must not run'); } },
  });
  props[Symbol('object-key')] = 'ignored';
  const tree = Object.create({ type: 'wrong', props: { inherited: 'ignored' } });
  tree.type = 'Text';
  tree.props = props;
  return tree;
}
pair('custom-prototype-getters-unsupported-values-array-extras', customDataTree(), customDataTree());
pair('primitive-props', { type: 'Text', props: 'plain' });
pair('null-props', { type: 'Text', props: null });
pair('bigint-props-omission', { type: 'Text', props: 1n });
pair('text-props-children-metadata', {
  type: 'Text', props: { children: { source: 'metadata' } },
});

const inheritedChildren = new Array(1);
const childrenPrototype = Object.create(Array.prototype);
Object.defineProperty(childrenPrototype, '0', {
  value: { type: 'Text', props: { label: 'inherited child' } }, enumerable: true,
});
Object.setPrototypeOf(inheritedChildren, childrenPrototype);
pair('children-inherited-index', { type: 'Box', props: {}, children: inheritedChildren });
pairedFailure('children-array-hole', { type: 'Box', props: {}, children: new Array(1) });

const nativeButton = {
  type: 'Button', props: { label: 'Save' }, hover: { borderColor: 'cyan' }, onPress() {}, children: [],
};
const hButton = {
  type: 'Button', props: { label: 'Save', onPress() {} }, hover: { borderColor: 'cyan' }, children: [],
};
pair('button-hover-held-children-order', nativeButton, hButton);
const nativeInput = {
  type: 'Input', props: { key: 'query' }, hover: { borderColor: 'cyan' }, onEvent() {}, children: [],
};
const hInput = {
  type: 'Input', props: { key: 'query', onSubmit() {}, onInput: undefined }, hover: { borderColor: 'cyan' }, children: [],
};
pair('input-optional-undefined-callback-omission', nativeInput, hInput);
const nativeSelect = {
  type: 'Select', props: { key: 'mode', options: [{ value: 'fast' }] }, hover: { borderColor: 'cyan' }, onEvent() {}, children: [],
};
const hSelect = {
  type: 'Select', props: { key: 'mode', options: [{ value: 'fast' }], onSelect() {} }, hover: { borderColor: 'cyan' }, children: [],
};
pair('select-hover-held-children-order', nativeSelect, hSelect);

pairedFailure('array-hole', { type: 'Text', props: { values: new Array(1) } });
let deep = 'leaf';
for (let index = 0; index < 34; index += 1) deep = { next: deep };
pairedFailure('props-depth', { type: 'Text', props: deep });
pairedFailure('unified-props-budget', {
  type: 'Text', props: { first: Array(9_999).fill(0), second: Array(9_999).fill(0) },
});

const inputBoundary = {
  type: 'Input',
  props: {
    key: 'query', onSubmit() {}, onInput: undefined,
    first: Array(9_999).fill(0), second: Array(9_997).fill(0),
  },
  children: [],
};
const nativeInputBoundary = {
  type: 'Input',
  props: { key: 'query', first: Array(9_999).fill(0), second: Array(9_997).fill(0) },
  onEvent() {}, children: [],
};
pair('input-callbacks-excluded-from-unified-budget', nativeInputBoundary, inputBoundary);

manager.dispose();
writeFileSync(
  new URL('./oracle-differential-results.jsonl', import.meta.url),
  results.map(result => JSON.stringify(result)).join('\n') + '\n',
);
console.log('source-oracle differential: 15 cases passed');
