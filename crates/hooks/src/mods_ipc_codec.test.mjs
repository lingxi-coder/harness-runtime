import assert from 'node:assert/strict';
import { test } from 'node:test';
import { decodeModsIpcFrame, encodeModsIpcFrame } from './mods_ipc_codec.mjs';

test('Mods IPC sidecar roundtrips exceptional numbers without marker collisions', () => {
  const userMarker = {
    protocol: 'lingxi.mods.ipc',
    version: 1,
    message: null,
    numberBits: [{ pointerUnits: [], bits: '7ff0000000000000' }],
  };
  const message = {
    id: 7,
    kind: 'result',
    payload: {
      nan: NaN,
      positiveInfinity: Infinity,
      negativeInfinity: -Infinity,
      negativeZero: -0,
      '/~': Infinity,
    },
    userMarker,
  };

  const decoded = decodeModsIpcFrame(encodeModsIpcFrame(message));
  assert(Number.isNaN(decoded.payload.nan));
  assert.equal(decoded.payload.positiveInfinity, Infinity);
  assert.equal(decoded.payload.negativeInfinity, -Infinity);
  assert(Object.is(decoded.payload.negativeZero, -0));
  assert.equal(decoded.payload['/~'], Infinity);
  assert.deepEqual(decoded.userMarker, userMarker);
});

test('Mods IPC replacer preserves JSON.stringify getter, toJSON, undefined, and cycle semantics', () => {
  let getterReads = 0;
  let toJsonCalls = 0;
  const message = {
    get nested() {
      getterReads += 1;
      return {
        toJSON() {
          toJsonCalls += 1;
          return { negativeZero: -0 };
        },
      };
    },
    omitted: undefined,
    array: [undefined, NaN],
  };

  const decoded = decodeModsIpcFrame(encodeModsIpcFrame(message));
  assert.equal(getterReads, 1);
  assert.equal(toJsonCalls, 1);
  assert(Object.is(decoded.nested.negativeZero, -0));
  assert.equal(Object.hasOwn(decoded, 'omitted'), false);
  assert.deepEqual(decoded.array, [null, NaN]);

  const cyclic = {};
  cyclic.self = cyclic;
  assert.throws(() => encodeModsIpcFrame(cyclic), /circular|cyclic/i);
});
