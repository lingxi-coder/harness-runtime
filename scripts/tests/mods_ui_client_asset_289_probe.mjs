#!/usr/bin/env node
// Extracts only the pinned Claude Code 2.1.289 hooks-types BunFS asset.
// Usage: node --experimental-vm-modules scripts/tests/mods_ui_client_asset_289_probe.mjs <binary>
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node --experimental-vm-modules scripts/tests/mods_ui_client_asset_289_probe.mjs <binary>');
const binary = readFileSync(binaryPath);
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const machO64Magic = 0xfeedfacf;
assert.equal(binary.readUInt32LE(0), machO64Magic, 'expected a little-endian 64-bit Mach-O');
const commandCount = binary.readUInt32LE(16);
let commandOffset = 32;
let bunSegment;
for (let index = 0; index < commandCount; index += 1) {
  const command = binary.readUInt32LE(commandOffset);
  const commandSize = binary.readUInt32LE(commandOffset + 4);
  assert(commandSize >= 8, `invalid Mach-O load command ${index}`);
  if (command === 0x19 && commandSize >= 72) { // LC_SEGMENT_64
    const segmentName = binary
      .toString('ascii', commandOffset + 8, commandOffset + 24)
      .replace(/\0.*$/, '');
    if (segmentName === '__BUN') {
      bunSegment = {
        fileOffset: Number(binary.readBigUInt64LE(commandOffset + 40)),
        fileSize: Number(binary.readBigUInt64LE(commandOffset + 48)),
      };
    }
  }
  commandOffset += commandSize;
}
assert.deepEqual(bunSegment, {fileOffset: 68_206_592, fileSize: 160_743_424});

const section = binary.subarray(
  bunSegment.fileOffset,
  bunSegment.fileOffset + bunSegment.fileSize,
);
const payloadLength = Number(section.readBigUInt64LE(0));
assert.equal(payloadLength, 160_738_634);
const graph = section.subarray(8, 8 + payloadLength);
const trailer = Buffer.from('\n---- Bun! ----\n');
assert(graph.subarray(-trailer.length).equals(trailer), 'standalone graph trailer mismatch');

// Bun's public standalone graph format puts a 32-byte Offsets record directly
// before the 16-byte trailer; offsets are relative to the graph body.
const offsetsAt = graph.length - 32 - trailer.length;
assert.equal(Number(graph.readBigUInt64LE(offsetsAt)), offsetsAt, 'graph byte_count mismatch');
const modulesOffset = graph.readUInt32LE(offsetsAt + 8);
const modulesLength = graph.readUInt32LE(offsetsAt + 12);
const entryPointId = graph.readUInt32LE(offsetsAt + 16);
const flags = graph.readUInt32LE(offsetsAt + 28);
assert.equal(entryPointId, 6);
assert.equal(flags, 0x1fff);
assert.equal(modulesLength % 52, 0, 'compiled module record table is malformed');
assert(modulesOffset + modulesLength <= offsetsAt, 'module table overlaps graph trailer');

const targetName = Buffer.from('/$bunfs/root/state-library.js-wy8y4843.txt');
let target;
for (let index = 0; index < modulesLength / 52; index += 1) {
  const recordAt = modulesOffset + index * 52;
  const fields = Array.from({length: 12}, (_, field) => graph.readUInt32LE(recordAt + field * 4));
  const [nameOffset, nameLength, contentsOffset, contentsLength] = fields;
  const name = graph.subarray(nameOffset, nameOffset + nameLength);
  if (!name.equals(targetName)) continue;
  assert.equal(target, undefined, 'duplicate hooks-types BunFS module record');
  target = {
    index,
    recordAt,
    contentsOffset,
    contentsLength,
    encoding: graph.readUInt8(recordAt + 48),
    loader: graph.readUInt8(recordAt + 49),
    moduleFormat: graph.readUInt8(recordAt + 50),
    side: graph.readUInt8(recordAt + 51),
  };
}
assert(target, 'pinned hooks-types asset not present in the standalone graph');
assert.deepEqual(target, {
  index: 2184,
  recordAt: 160_706_581,
  contentsOffset: 152_588_771,
  contentsLength: 4_060,
  encoding: 1,
  loader: 13,
  moduleFormat: 0,
  side: 1,
});
assert(target.contentsOffset + target.contentsLength <= offsetsAt);
const assetBytes = graph.subarray(
  target.contentsOffset,
  target.contentsOffset + target.contentsLength,
);
const assetSha256 = sha256(assetBytes);
assert.equal(assetSha256, '7908c3310f4764acd78ef09276fc3ed5c1315e456b5aa95aa18363b8baf87b55');
assert(assetBytes.every((byte) => byte < 0x80), 'asset was expected to contain ASCII source bytes');
const assetSource = assetBytes.toString('ascii');

assert.equal(typeof vm.SourceTextModule, 'function', 'run Node with --experimental-vm-modules');
const context = vm.createContext(Object.create(null));
const hooksTypes = new vm.SourceTextModule(assetSource, {
  context,
  identifier: 'claude:hooks-types',
});
await hooksTypes.link(() => {
  throw new Error('unexpected import in the embedded hooks-types source');
});
await hooksTypes.evaluate();
const runtimeExports = Object.keys(hooksTypes.namespace).sort();
assert.deepEqual(runtimeExports, ['atom', 'derive', 'memberOf', 'read', 'update']);

const {atom, derive, memberOf, read, update} = hooksTypes.namespace;
const stateKey = ref => JSON.stringify([ref.plugin, ref.key, ref.id]);
function memoryState() {
  const rows = new Map();
  const writes = [];
  return {
    rows,
    writes,
    async get(ref) {
      return rows.get(stateKey(ref)) ?? {value: undefined, version: 0};
    },
    async set(ref, value, options) {
      writes.push({ref: {...ref}, value, options});
      const key = stateKey(ref);
      const current = rows.get(key) ?? {value: undefined, version: 0};
      if (options?.ifVersion !== current.version) return {isSet: false};
      rows.set(key, {value, version: current.version + 1});
      return {isSet: true};
    },
  };
}
const asJson = value => JSON.parse(JSON.stringify(value));

// Atom ownership is a frozen plugin/key/id reference; extra reference keys are
// discarded, initial values are recursively frozen in place, and shape tags
// make stale stored values fall back to the declared initial value.
const initial = {count: 1, nested: {ok: true}};
const counter = atom(
  {plugin: 'plug', key: 'counter', id: 'row-1', ignored: true},
  initial,
  {shape: 'counter-v1'},
);
const counterValue = {
  enumerable: JSON.parse(JSON.stringify(counter)),
  atomFrozen: Object.isFrozen(counter),
  refFrozen: Object.isFrozen(counter.ref),
  initialFrozen: Object.isFrozen(initial),
  nestedFrozen: Object.isFrozen(initial.nested),
};
assert.deepEqual(counterValue, {
  enumerable: {
    ref: {plugin: 'plug', key: 'counter', id: 'row-1'},
    initial: {count: 1, nested: {ok: true}},
    shape: 'counter-v1',
  },
  atomFrozen: true,
  refFrozen: true,
  initialFrozen: true,
  nestedFrozen: true,
});

const counterState = memoryState();
counterState.rows.set(stateKey(counter.ref), {
  value: {shape: 'counter-old', value: {count: 99}},
  version: 3,
});
const counterApi = {state: counterState};
const shapedFallbackRead = await read(counterApi, counter);
const shapedUpdate = await update(counterApi, counter, value => ({count: value.count + 1}));
const shapedWrite = counterState.rows.get(stateKey(counter.ref));
assert.deepEqual(asJson(shapedFallbackRead), {count: 1, nested: {ok: true}});
assert.deepEqual(asJson(shapedUpdate), {count: 2});
assert.deepEqual(asJson(shapedWrite), {
  value: {shape: 'counter-v1', value: {count: 2}},
  version: 4,
});
assert.deepEqual(asJson(counterState.writes[0].options), {ifVersion: 3});

const versionlessWrites = [];
const versionlessState = {
  value: undefined,
  async get() { return {value: this.value, version: undefined}; },
  async set(ref, value, options) {
    versionlessWrites.push({
      ref: asJson(ref),
      value,
      hasIfVersion: Object.hasOwn(options, 'ifVersion'),
      ifVersion: options.ifVersion,
    });
    this.value = value;
    return {isSet: true};
  },
};
const unshaped = atom({plugin: 'plug', key: 'versionless'}, 'seed');
const versionlessRead = await read({state: versionlessState}, unshaped);
const versionlessUpdate = await update(
  {state: versionlessState},
  unshaped,
  value => `${value}-updated`,
);
const rawUndefinedRead = await read(
  {state: {async get() { return {value: undefined, version: undefined}; }}},
  {plugin: 'plug', key: 'missing-raw'},
);
assert.equal(versionlessRead, 'seed');
assert.equal(versionlessUpdate, 'seed-updated');
assert.deepEqual(versionlessWrites, [{
  ref: {plugin: 'plug', key: 'versionless'},
  value: 'seed-updated',
  hasIfVersion: true,
  ifVersion: undefined,
}]);
assert.equal(rawUndefinedRead, undefined);

// Derive keeps its dependency order, flattens versions in that order, and
// recomputes only when the resulting joined version key changes.
const leftRef = {plugin: 'plug', key: 'left', ignored: 1};
const rightRef = {plugin: 'plug', key: 'right'};
const derivedState = memoryState();
derivedState.rows.set(stateKey(leftRef), {value: 2, version: 5});
derivedState.rows.set(stateKey(rightRef), {value: 4, version: 8});
const computeInputs = [];
const total = derive([leftRef, rightRef], (left, right) => {
  computeInputs.push([left, right]);
  return left + right;
});
const derivedApi = {state: derivedState};
const derivedValues = [
  await read(derivedApi, total),
  await read(derivedApi, total),
];
derivedState.rows.set(stateKey(rightRef), {value: 7, version: 9});
derivedValues.push(await read(derivedApi, total));
assert.deepEqual(asJson(total.sources), [
  {plugin: 'plug', key: 'left'},
  {plugin: 'plug', key: 'right'},
]);
assert(Object.isFrozen(total) && Object.isFrozen(total.sources));
assert.deepEqual(derivedValues, [6, 6, 9]);
assert.deepEqual(computeInputs, [[2, 4], [2, 7]]);

const family = atom(
  {plugin: 'plug', key: 'member', id: 'family-default', ignored: true},
  {value: 0},
  {shape: 'member-v1'},
);
const scopedMember = memberOf(family, {requestId: 'request-42'});
const defaultMember = memberOf(family, {});
const rawMember = memberOf(
  {plugin: 'plug', key: 'raw-member', id: 'raw-default', ignored: true},
  {requestId: 'request-43'},
);
assert.deepEqual(asJson(scopedMember.ref), {plugin: 'plug', key: 'member', id: 'request-42'});
assert.deepEqual(asJson(defaultMember.ref), {plugin: 'plug', key: 'member', id: 'family-default'});
assert.deepEqual(asJson(rawMember), {plugin: 'plug', key: 'raw-member', id: 'request-43'});
assert(Object.isFrozen(scopedMember) && Object.isFrozen(scopedMember.ref));

// On a CAS conflict update rereads and reruns the transform; after 64 failed
// writes it throws the Native error without reporting a value as committed.
const retryRef = {plugin: 'plug', key: 'retry'};
let retryReadCount = 0;
const retrySeen = [];
const retryState = {
  async get() {
    retryReadCount += 1;
    if (retryReadCount === 1) return {value: 3, version: 1};
    return {value: 10, version: 2};
  },
  async set(_ref, value, options) {
    if (options.ifVersion === 1) return {isSet: false};
    assert.deepEqual(value, 11);
    return {isSet: true};
  },
};
const retryReturn = await update({state: retryState}, retryRef, value => {
  retrySeen.push(value);
  return value + 1;
});
assert.equal(retryReturn, 11);
assert.deepEqual(retrySeen, [3, 10]);

let exhaustedChanges = 0;
let exhaustedWrites = 0;
let exhaustedError;
try {
  await update({state: {
    async get() { return {value: 'v', version: 7}; },
    async set() { exhaustedWrites += 1; return {isSet: false}; },
  }}, {plugin: 'plug', key: 'contended'}, value => {
    exhaustedChanges += 1;
    return `${value}!`;
  });
} catch (error) {
  exhaustedError = error.message;
}
assert.equal(exhaustedChanges, 64);
assert.equal(exhaustedWrites, 64);
assert.equal(
  exhaustedError,
  'update: the value was written by another every time it was read, up to the bound on tries; nothing was written',
);

const hooksTypesContract = {
  counter: counterValue,
  shapedFallbackRead,
  shapedUpdate,
  shapedWrite,
  shapedWriteIfVersion: counterState.writes[0].options.ifVersion,
  versionless: {
    read: versionlessRead,
    updated: versionlessUpdate,
    write: versionlessWrites[0],
    writeIfVersionIsUndefined: versionlessWrites[0].ifVersion === undefined,
    rawRead: rawUndefinedRead,
    rawReadIsUndefined: rawUndefinedRead === undefined,
  },
  derivedValues,
  deriveCalls: computeInputs,
  memberRefs: [scopedMember.ref, defaultMember.ref, rawMember],
  retry: {return: retryReturn, inputs: retrySeen},
  exhausted: {changes: exhaustedChanges, writes: exhaustedWrites, error: exhaustedError},
};
const hooksTypesContractSha256 = sha256(Buffer.from(JSON.stringify(hooksTypesContract), 'utf8'));
assert.equal(
  hooksTypesContractSha256,
  'bf6c66bf48aac059cee43b106610edae0439aebd970de1442fac651557dc938c',
  'Native hooks-types observable contract fixture changed',
);

console.log(JSON.stringify({
  nativeSha256: sha256(binary),
  bunSegment,
  bunGraphPayloadLength: payloadLength,
  moduleTableCount: modulesLength / 52,
  asset: {
    name: targetName.toString('ascii'),
    moduleIndex: target.index,
    recordOffset: target.recordAt,
    contentsOffset: target.contentsOffset,
    contentsLength: target.contentsLength,
    sha256: assetSha256,
    runtimeExports,
  },
  hooksTypesContract,
  hooksTypesContractSha256,
}, null, 2));
