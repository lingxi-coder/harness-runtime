#!/usr/bin/env node
// Focused Native 2.1.289 ref coercion and $.tool.call denial projection probe.
// This executes pinned helper slices with controlled run/tool inputs; it does
// not execute the Native tool catalog, permission UI, or external tools.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node scripts/tests/mods_tool_call_ref_289_probe.mjs <2.1.289-binary>');
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const binary = readFileSync(binaryPath);
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const chunks = {
  helpers: {
    start: 182637043,
    end: 182864930,
    length: 227886,
    sha256: 'b8b83549f7b4273ed3f6b4669d666e535f9abfd831ea3952e9387ac73500e85c',
  },
  primary: {
    start: 187181803,
    end: 191691521,
    length: 4509717,
    sha256: '47b62b9ac561f0cc5cd75ab6e7b9e14c1b027cccebfb900b56f07685c69fa825',
  },
};

function chunk(name) {
  const pin = chunks[name];
  const raw = binary.subarray(pin.start, pin.end);
  assert.equal(raw.at(-1), 0, `${name}: expected trailing NUL`);
  const source = raw.subarray(0, -1);
  assert.equal(source.length, pin.length, `${name}: source length changed`);
  assert.equal(sha256(source), pin.sha256, `${name}: source chunk hash changed`);
  assert.deepEqual(binary.subarray(pin.start, pin.end - 1), source,
    `${name}: source mapping no longer matches executable bytes`);
  return source;
}

const helpers = chunk('helpers');
const primary = chunk('primary');
function slice(bytes, start, end, expectedHash, marker) {
  const source = bytes.subarray(start, end);
  assert.equal(source.length, end - start, 'pinned Native source range length changed');
  assert.equal(sha256(source), expectedHash, 'pinned Native source range hash changed');
  if (marker) assert(source.toString().startsWith(marker), `expected source marker ${marker}`);
  return source.toString();
}

const keSource = slice(helpers, 13822, 14002,
  '08a9bf9892e71e9249205bc9b9b5f5e0c2a9336f3d8d8885686326e5ca88e225', 'function Ke(');
const snSource = slice(helpers, 98452, 98722,
  'cd86a3640f57c7f3ea299598e2722b567205c08f60e6b3a0f183fba90091ad50', 'function Sn(');
const checkSource = slice(helpers, 98962, 99316,
  'fcb695c8551a2b84336af2f788130a68d1c83c9783531a35fb57ee43be70aa60', 'var ma=');
const xiSource = slice(helpers, 126157, 126322,
  'bcb84d348c2772be59766f01ba7d207eca4e1dc1da9ecdec630aa5268f979294', 'function XIe(');

const wuSource = slice(primary, 555917, 556029,
  '086a383343a8675cd9ece487418c323fa8e3035e071462a6574347d1fece5e4d', 'var wUn=');
const vuSource = slice(primary, 556029, 556131,
  '31df52258cb1479b8ae29ca06ab2990da7383ced46550b116e37c30d3d53dc60', 'var vUn=');
const rceSource = slice(primary, 557418, 557522,
  'db18518faacc62b346b33e21ad5cf910d069b559026fab8066449ea97512af77', 'var RCe=');
const dvtSource = slice(primary, 2382584, 2384735,
  '13894e860503c02698c05aa63d857eecb7ce51f0bb32486c59b10809761adf15', 'async function DVt(');
const reSource = slice(primary, 4130506, 4132022,
  '643c2975cfe603827a217f3ebc358cdd155416c704bd0d6b37bcd0a25de1e33f', 'function Re(');

const shared = {
  A: fn => fn,
  Ke: undefined,
  _s: () => undefined,
  nL: () => ({}), Ad: {}, Ft: () => {}, wd: [],
  Sn: undefined, On: value => value, Ot: () => {}, q4r: () => {},
  Bn: value => JSON.stringify(value),
  LG: () => undefined, Z8: () => false, AZe: () => undefined,
  iu: [], vg: () => 'native-probe-uuid',
};
shared.Ke = vm.runInNewContext(`${keSource}; Ke`, shared);
shared.Sn = vm.runInNewContext(`${snSource}; Sn`, shared);
const nativeValidator = vm.runInNewContext(`${checkSource}; ma`, shared);
const newLedger = vm.runInNewContext(`${xiSource}; XIe`, shared);

assert.equal(nativeValidator.check({ref: {}, result: null}), undefined,
  'public validator unexpectedly restricted ref type or explicit null result');
assert.equal(nativeValidator.check({ref: true, result: null}), undefined);
assert.equal(nativeValidator.check({ref: [1], result: null}), undefined);
assert.equal(nativeValidator.check({ref: 0}), 'neither { result } nor { deny }',
  'public validator must reject a missing result when no deny exists');
assert.equal(nativeValidator.check({ref: 0, deny: 'blocked'}), undefined,
  'deny without result is a valid public answer');

const stripped = shared.Sn(
  {ref: '1', result: null, isReadOnly: true},
  [{ref: 1, result: null, isReadOnly: true}],
);
assert.equal(stripped.isReadOnly, undefined,
  'Native readonly carry must compare refs by strict equality');

const ledger = newLedger();
ledger.keep({name: 'first'}, {result: 'first', messages: [{id: 'first'}]});
ledger.keep({name: 'second'}, {result: 'second', messages: [{id: 'second'}]});
const refs = [true, false, [1], [2], [], {}, null, 0, 99, 1.5, 'bad', '1', '2'];
const selected = refs.map(ref => ledger.of(ref)?.input.name ?? null);
assert.deepEqual(selected, [
  'first', null, 'first', 'second', null, null, null, null, null, null,
  null, 'first', 'second',
]);

const re = vm.runInNewContext(`${reSource}; Re`, shared);
const wUn = vm.runInNewContext(`${wuSource}; wUn`, shared);
const RCe = vm.runInNewContext(`${rceSource}; RCe`, {...shared, Re: re, wUn});
const vUn = vm.runInNewContext(`${vuSource}; vUn`, {...shared, Bn: shared.Bn});
const DVt = vm.runInNewContext(`${dvtSource}; DVt`, {
  ...shared,
  DOe: () => {},
  vUn,
  _O: () => false,
  t: () => {},
  RCe,
  OVt: () => [],
  I$: () => true,
  dlt: () => false,
  _se: () => ({}),
  kce: async () => ({messages: [], rewritten: false}),
  slt: () => 0,
  WUe: async (_tool, output, id) => ({
    type: 'tool_result', content: String(output), is_error: false, tool_use_id: id,
  }),
  F_: () => undefined,
  Re: re,
  RUn: (_rows, _id, block) => ({content: [block]}),
  PUn: () => [],
  CE: () => undefined,
});

function inputs(answer, runs) {
  return [answer, {
    runs,
    thrown: () => undefined,
    managedRan: () => false,
    undelivered: () => [],
  }, {
    tool: {name: 'Read'},
    input: {tool_use_id: 'probe-tool-use'},
    toolUseContext: {
      abortController: {signal: {aborted: false}},
      options: {bareFork: false, settingsHooksOff: false},
      session: {},
      storageV5: false,
    },
    assistantMessage: {uuid: 'virtual-assistant-id'},
    now: () => '2026-10-04T00:00:00.000Z',
  }];
}

const passLedger = newLedger();
passLedger.keep({name: 'first'}, {result: 'first', messages: [{id: 'first'}]});
const pass = await DVt(...inputs({ref: true, result: 'first'}, passLedger));
assert.deepEqual(JSON.parse(JSON.stringify(pass)), [{id: 'first'}],
  'coercible selected ref should pass through the Native run');

const missLedger = newLedger();
missLedger.keep({name: 'first'}, {result: 'first', messages: [{id: 'first'}]});
const replacement = await DVt(...inputs({ref: 0, result: null}, missLedger));
assert.equal(replacement.length, 1);
assert.equal(replacement[0].message.toolUseResult, null,
  'a reachable explicit-null result must remain distinct from a missing property');

const denied = await DVt(...inputs({ref: true, deny: 'blocked'}, passLedger));
const denialRow = denied[0].message;
assert.equal(denialRow.type, 'user');
assert.equal(denialRow.toolUseResult, 'Error: blocked');
assert.equal(denialRow.toolDenialKind, 'permission-rule');
assert.equal(denialRow.sourceToolAssistantUUID, 'virtual-assistant-id');
assert.equal(denialRow.isError, undefined);
assert.equal(denialRow.mcpMeta, undefined);
assert.equal(denialRow.toolEndsTurn, undefined);
assert.deepEqual(JSON.parse(JSON.stringify(denialRow.message.content[0])), {
  type: 'tool_result',
  content: '<tool_use_error>blocked</tool_use_error>',
  is_error: true,
  tool_use_id: 'probe-tool-use',
});

console.log('passed 2.1.289 public tool.call validator, ref coercion, pass-through, replacement and denial projections (controlled Native slices only)');
