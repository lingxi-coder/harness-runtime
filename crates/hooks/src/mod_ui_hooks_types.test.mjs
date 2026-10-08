import assert from 'node:assert/strict';
import { test } from 'node:test';
import { atom, derive, memberOf, read, update } from './mod_ui_hooks_types.mjs';

function stateKey(ref) {
  return JSON.stringify(ref);
}

test('atom copies only its state reference fields and deeply freezes its initial value', () => {
  const initial = { nested: [{ count: 1 }] };
  const value = atom({ plugin: 'plugin-a', key: 'settings', id: 'row-1', ignored: true }, initial, {
    shape: 'v2', ignored: true,
  });

  assert.deepEqual(JSON.parse(JSON.stringify(value)), {
    ref: { plugin: 'plugin-a', key: 'settings', id: 'row-1' },
    initial: { nested: [{ count: 1 }] },
    shape: 'v2',
  });
  assert.deepEqual(Object.keys(value), ['ref', 'initial', 'shape']);
  assert(Object.isFrozen(value));
  assert(Object.isFrozen(value.ref));
  assert(Object.isFrozen(value.initial));
  assert(Object.isFrozen(value.initial.nested));
  assert(Object.isFrozen(value.initial.nested[0]));
  const atomBrand = Symbol.for('claude-code.state.atom');
  assert.equal(value[atomBrand], true);
  assert.equal(Object.getOwnPropertyDescriptor(value, atomBrand).enumerable, true);

  const mutableNested = { value: 1 };
  const preFrozen = Object.freeze({ nested: mutableNested });
  const alreadyFrozenAtom = atom({ plugin: 'plugin-a', key: 'pre-frozen' }, preFrozen);
  assert(Object.isFrozen(alreadyFrozenAtom.initial));
  assert.equal(Object.isFrozen(mutableNested), false,
    'the Native deep-freezer returns early for an already-frozen parent');
});

test('memberOf preserves atom defaults and shape, while raw refs stay raw and frozen', () => {
  const family = atom({ plugin: 'plugin-a', key: 'drafts', id: 'family' }, { text: '' }, {
    shape: 'draft-v1',
  });
  const member = memberOf(family, { requestId: 'message-7', ignored: true });
  assert.deepEqual(JSON.parse(JSON.stringify(member)), {
    ref: { plugin: 'plugin-a', key: 'drafts', id: 'message-7' },
    initial: { text: '' },
    shape: 'draft-v1',
  });
  assert(Object.isFrozen(member));

  const defaultId = memberOf(family, {});
  assert.equal(defaultId.ref.id, 'family');
  const explicitNull = memberOf(family, { requestId: null });
  assert.equal(explicitNull.ref.id, null);

  const raw = memberOf({ plugin: 'plugin-a', key: 'drafts', id: 'family', extra: true }, {});
  assert.deepEqual(raw, { plugin: 'plugin-a', key: 'drafts', id: 'family' });
  assert(Object.isFrozen(raw));
  assert.equal(member[Symbol.for('claude-code.state.atom')], true,
    'memberOf retains the atom brand when cloning an atom');
});

test('read applies atom initial and shape rules but returns raw ref values unchanged', async () => {
  const values = new Map();
  const $ = { state: { get: async ref => values.get(stateKey(ref)) } };
  const ref = { plugin: 'plugin-a', key: 'settings' };
  const regular = atom(ref, { count: 0 });
  const shaped = atom(ref, { count: -1 }, { shape: 'v2' });

  assert.deepEqual(await read($, regular), { count: 0 });
  assert.deepEqual(await read($, shaped), { count: -1 });
  values.set(stateKey(ref), { value: { count: 8 }, version: 1 });
  assert.deepEqual(await read($, regular), { count: 8 });
  assert.deepEqual(await read($, shaped), { count: -1 });
  values.set(stateKey(ref), { value: { shape: 'v2', value: { count: 9 } }, version: 2 });
  assert.deepEqual(await read($, shaped), { count: 9 });
  assert.deepEqual(await read($, ref), { shape: 'v2', value: { count: 9 } });

  const requests = [];
  const forgedAtom = { ref, initial: { count: 77 } };
  const forgedDerived = { sources: [], compute: () => 'forged' };
  const rawApi = { state: { get: async source => {
    requests.push(source);
    return { value: 'raw-ref-value', version: 3 };
  } } };
  assert.equal(await read(rawApi, forgedAtom), 'raw-ref-value');
  assert.strictEqual(requests[0], forgedAtom,
    'shape fields alone do not brand an atom or sanitize a direct raw-ref read');
  assert.equal(await read(rawApi, forgedDerived), 'raw-ref-value');
  assert.strictEqual(requests[1], forgedDerived,
    'shape fields alone do not brand a derived source');
});

test('derive freezes normalized sources and memoizes until a source version changes', async () => {
  const values = new Map([
    ['a', { value: 2, version: 1 }],
    ['b', { value: 5, version: 4 }],
  ]);
  const $ = { state: { get: async ref => values.get(ref.key) } };
  const a = atom({ plugin: 'plugin-a', key: 'a' }, 0);
  const b = { plugin: 'plugin-a', key: 'b', ignored: true };
  let computes = 0;
  const total = derive([a, b], (left, right) => {
    computes += 1;
    return left + right;
  });

  assert.deepEqual(Object.keys(total), ['sources', 'compute']);
  assert(Object.isFrozen(total));
  assert(Object.isFrozen(total.sources));
  assert(Object.isFrozen(total.sources[0]));
  assert.deepEqual(total.sources[1], { plugin: 'plugin-a', key: 'b' });
  assert.equal(await read($, total), 7);
  assert.equal(await read($, total), 7);
  assert.equal(computes, 1);

  values.set('b', { value: 8, version: 5 });
  assert.equal(await read($, total), 10);
  assert.equal(computes, 2);
});

test('update retries compare-and-set conflicts and shape-wraps only stored values', async () => {
  const ref = { plugin: 'plugin-a', key: 'counter' };
  const target = atom(ref, 0, { shape: 'counter-v1' });
  let record = { value: { shape: 'counter-v1', value: 1 }, version: 3 };
  const seen = [];
  let writes = 0;
  const $ = { state: {
    get: async requested => {
      assert.deepEqual(requested, ref);
      return record;
    },
    set: async (requested, value, options) => {
      assert.deepEqual(requested, ref);
      writes += 1;
      seen.push({ value, ifVersion: options.ifVersion });
      if (writes === 1) {
        record = { value: record.value, version: 4 };
        return { isSet: false };
      }
      assert.equal(options.ifVersion, 4);
      record = { value, version: 5 };
      return { isSet: true };
    },
  } };
  const observed = [];
  const result = await update($, target, current => {
    observed.push(current);
    return current + 1;
  });

  assert.equal(result, 2);
  assert.deepEqual(observed, [1, 1]);
  assert.deepEqual(seen, [
    { value: { shape: 'counter-v1', value: 2 }, ifVersion: 3 },
    { value: { shape: 'counter-v1', value: 2 }, ifVersion: 4 },
  ]);
  assert.deepEqual(record, { value: { shape: 'counter-v1', value: 2 }, version: 5 });
});

test('update reports the Native retry-bound error after 64 compare-and-set conflicts', async () => {
  let reads = 0;
  let writes = 0;
  const $ = { state: {
    get: async () => { reads += 1; return { value: 0, version: reads }; },
    set: async () => { writes += 1; return { isSet: false }; },
  } };
  await assert.rejects(update($, { plugin: 'plugin-a', key: 'counter' }, value => value + 1), {
    message: 'update: the value was written by another every time it was read, up to the bound on tries; nothing was written',
  });
  assert.equal(reads, 64);
  assert.equal(writes, 64);
});

test('update passes a Promise from change directly to state.set without awaiting the callback', async () => {
  const next = Promise.resolve('next-value');
  let stored;
  const $ = { state: {
    get: async () => ({ value: 'current', version: 1 }),
    set: async (_ref, value) => {
      stored = value;
      return { isSet: true };
    },
  } };
  const result = await update($, { plugin: 'plugin-a', key: 'value' }, () => next);
  assert.strictEqual(stored, next);
  assert.equal(result, 'next-value', 'the async update return adopts the Promise result');
});
