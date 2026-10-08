#!/usr/bin/env node
// Compare the small functional port with the hooks-types module embedded in
// the pinned Claude Code 2.1.289 executable. No Native source is copied here.
// Usage: node --experimental-vm-modules scripts/tests/mods_ui_hooks_types_289_oracle.mjs <binary>
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import * as port from '../../crates/hooks/src/mod_ui_hooks_types.mjs';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node --experimental-vm-modules scripts/tests/mods_ui_hooks_types_289_oracle.mjs <binary>');
const binary = readFileSync(binaryPath);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256');

const section = binary.subarray(68_206_592, 68_206_592 + 160_743_424);
const graphLength = Number(section.readBigUInt64LE(0));
assert.equal(graphLength, 160_738_634);
const graph = section.subarray(8, 8 + graphLength);
const asset = graph.subarray(152_588_771, 152_588_771 + 4_060);
assert.equal(sha256(asset), '7908c3310f4764acd78ef09276fc3ed5c1315e456b5aa95aa18363b8baf87b55',
  'unexpected Native hooks-types source asset');
assert(asset.every(byte => byte < 0x80), 'Native hooks-types asset must be ASCII');

const context = vm.createContext(Object.create(null));
const nativeModule = new vm.SourceTextModule(asset.toString('ascii'), {
  context,
  identifier: 'claude:hooks-types',
});
await nativeModule.link(() => { throw new Error('unexpected hooks-types import'); });
await nativeModule.evaluate();
const native = nativeModule.namespace;
assert.deepEqual(Object.keys(native).sort(), ['atom', 'derive', 'memberOf', 'read', 'update']);
assert.deepEqual(Object.keys(port).sort(), Object.keys(native).sort());

function json(value) {
  return value === undefined ? '<undefined>' : JSON.stringify(value);
}

function makeState(initial, { conflictOnce = false, alwaysConflict = false } = {}) {
  const records = new Map(Object.entries(initial));
  const trace = [];
  let setCalls = 0;
  const api = { state: {
    async get(ref) {
      const record = records.get(ref.key);
      trace.push(['get', ref, record]);
      return record === undefined ? { value: undefined, version: undefined } : { ...record };
    },
    async set(ref, value, options) {
      setCalls += 1;
      trace.push(['set', ref, value, options, Object.hasOwn(options, 'ifVersion')]);
      const current = records.get(ref.key) ?? { value: undefined, version: undefined };
      if (alwaysConflict || conflictOnce && setCalls === 1) {
        records.set(ref.key, { value: 10, version: 2 });
        return { isSet: false };
      }
      if (options.ifVersion !== current.version) return { isSet: false };
      records.set(ref.key, { value, version: (current.version ?? 0) + 1 });
      return { isSet: true };
    },
  } };
  return { api, records, trace, get setCalls() { return setCalls; } };
}

async function runCases(library) {
  const atomBrand = Symbol.for('claude-code.state.atom');
  const derivedBrand = Symbol.for('claude-code.state.derived');
  const initial = { count: 1, nested: { ok: true } };
  const family = library.atom({ plugin: 'plug', key: 'counter', id: 'family-default', extra: true },
    initial, { shape: 'counter-v1', ignored: true });
  const shapeState = makeState({ counter: {
    value: { shape: 'counter-old', value: { count: 99 } }, version: 3,
  } });
  const fallbackRead = await library.read(shapeState.api, family);
  shapeState.records.set('counter', {
    value: { shape: 'counter-v1', value: { count: 6 } }, version: 4,
  });
  const shapedRead = await library.read(shapeState.api, family);

  const member = library.memberOf(family, { requestId: 'request-42' });
  const defaultMember = library.memberOf(family, {});
  const rawMember = library.memberOf({ plugin: 'plug', key: 'raw', id: 'base', extra: true }, {
    requestId: 'request-43',
  });

  const deriveState = makeState({
    left: { value: 2, version: 5 },
    right: { value: 4, version: 8 },
  });
  const left = library.atom({ plugin: 'plug', key: 'left' }, 0);
  const right = { plugin: 'plug', key: 'right', ignored: true };
  const computes = [];
  const total = library.derive([left, right], (a, b) => {
    computes.push([a, b]);
    return a + b;
  });
  const first = await library.read(deriveState.api, total);
  const sameVersion = await library.read(deriveState.api, total);
  deriveState.records.set('right', { value: 7, version: 9 });
  const changedVersion = await library.read(deriveState.api, total);

  const updateState = makeState({ counter: {
    value: { shape: 'counter-v1', value: { count: 1 } }, version: 3,
  } });
  const updated = await library.update(updateState.api, family,
    value => ({ count: value.count + 1 }));

  const conflictState = makeState({ counter: { value: 3, version: 1 } }, { conflictOnce: true });
  const conflictInputs = [];
  const conflictResult = await library.update(conflictState.api,
    { plugin: 'plug', key: 'counter' }, value => {
      conflictInputs.push(value);
      return value + 1;
    });

  const missingState = makeState({ counter: { value: undefined, version: undefined } });
  const missingAtom = library.atom({ plugin: 'plug', key: 'counter' }, 'seed');
  const missingRead = await library.read(missingState.api, missingAtom);
  const rawMissingRead = await library.read(missingState.api, { plugin: 'plug', key: 'counter' });
  const missingUpdate = await library.update(missingState.api, missingAtom,
    value => `${value}-updated`);

  const failedState = makeState({ counter: { value: 0, version: 0 } }, { alwaysConflict: true });
  let failedTransforms = 0;
  let failure;
  try {
    await library.update(failedState.api, { plugin: 'plug', key: 'counter' }, value => {
      failedTransforms += 1;
      return value + 1;
    });
  } catch (error) {
    failure = { name: error.name, message: error.message };
  }

  return {
    atom: {
      value: family,
      frozen: Object.isFrozen(family) && Object.isFrozen(family.ref)
        && Object.isFrozen(family.initial) && Object.isFrozen(family.initial.nested),
      branded: family[atomBrand] === true,
      enumerableBrand: Object.getOwnPropertyDescriptor(family, atomBrand)?.enumerable === true,
    },
    shapeReads: [fallbackRead, shapedRead],
    members: [member, defaultMember, rawMember],
    memberBranded: member[atomBrand] === true,
    derived: { first, sameVersion, changedVersion, computes, sourceFrozen: Object.isFrozen(total.sources)
      && Object.isFrozen(total.sources[1]), branded: total[derivedBrand] !== undefined,
      enumerableBrand: Object.getOwnPropertyDescriptor(total, derivedBrand)?.enumerable === true,
      mutableMemo: !Object.isFrozen(total[derivedBrand]) },
    update: { value: updated, record: updateState.records.get('counter'), trace: updateState.trace },
    conflict: { value: conflictResult, inputs: conflictInputs, record: conflictState.records.get('counter') },
    missing: { read: missingRead, update: missingUpdate, rawRead: rawMissingRead,
      ifVersionPresent: missingState.trace.find(item => item[0] === 'set')?.[4] },
    exhaustion: { failure, writes: failedState.setCalls, transforms: failedTransforms },
  };
}

const nativeResult = await runCases(native);
const portResult = await runCases(port);
assert.equal(json(portResult), json(nativeResult), 'functional port differs from pinned Native behavior');

console.log(JSON.stringify({
  nativeBinarySha256: sha256(binary),
  hooksTypesAssetSha256: sha256(asset),
  exports: Object.keys(native).sort(),
  compared: ['atom', 'memberOf', 'read', 'derive memo/version changes', 'update CAS retry', 'undefined version', '64-conflict bound'],
  resultSha256: sha256(Buffer.from(json(nativeResult))),
  passed: true,
}, null, 2));
