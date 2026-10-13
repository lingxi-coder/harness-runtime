import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { decodeModsIpcFrame, encodeModsIpcFrame } from './mods_ipc_codec.mjs';

async function startWorker(t) {
  const root = await mkdtemp(join(tmpdir(), 'mods-skill-prompt-'));
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, [workerPath], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(async () => {
    if (!worker.killed) worker.kill();
    await rm(root, { recursive: true, force: true });
  });

  const pending = new Map();
  const pendingCoreEventNames = [];
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = decodeModsIpcFrame(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({
        id: frame.id, kind: 'api.callers.result', callId: frame.callId,
      })}\n`);
      return;
    }
    if (frame.kind === 'next') {
      pendingCoreEventNames.push(frame.event);
      worker.stdin.write(`${encodeModsIpcFrame({
        id: frame.id, kind: 'next.result', callId: frame.callId,
        result: { text: frame.input.text },
      })}\n`);
      return;
    }
    if (frame.id !== 0 && frame.kind !== 'progress') {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', () => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit' });
    pending.clear();
  });

  let nextId = 1;
  const request = message => new Promise((resolve, reject) => {
    const id = nextId++;
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
  });

  async function load(source, tier = 'user') {
    const module = join(root, 'skill-prompt.mjs');
    await writeFile(module, source);
    const prepared = await request({
      kind: 'prepare', root, module,
      sourceGraph: {
        entry: module,
        compilerVersion: 'skill.prompt worker test sourceGraph (plain JavaScript passthrough)',
        files: [{ file: module, source }],
        links: [],
      },
    });
    assert.equal(prepared.kind, 'prepared', JSON.stringify(prepared));
    const loaded = await request({
      kind: 'load', root, module, plugin: 'skill-prompt-test',
      storageId: 'skill-prompt-test@user', tier, preparedToken: prepared.token,
    });
    assert.equal(loaded.kind, 'loaded', JSON.stringify(loaded));
  }

  return { load, request, pendingCoreEventNames };
}

test('skill.prompt uses exact skill matching and preserves UTF-16 source strings', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, (_api, event) => ({ text: 'seen:' + event.text }));
      on('skill.prompt', { skill: 'insights\uD800' }, (_api, event) => ({ text: event.text + '\uD800' }));
    }
  `);

  const matched = await request({
    kind: 'dispatch', event: 'skill.prompt',
    input: { skill: 'insights', text: 'hello' },
  });
  assert.equal(matched.kind, 'result', JSON.stringify(matched));
  assert.deepEqual(matched.result, { text: 'seen:hello' });
  assert.deepEqual(matched.hooked, ['skill-prompt-test']);

  const noHit = await request({
    kind: 'dispatch', event: 'skill.prompt',
    input: { skill: 'Insights', text: 'hello' },
  });
  assert.equal(noHit.kind, 'result', JSON.stringify(noHit));
  assert.deepEqual(noHit.result, { text: 'hello' });
  assert.equal(Object.hasOwn(noHit, 'hooked'), false);

  const skill = 'insights\uD800';
  const text = 'prompt\uD800tail';
  const utf16 = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill, text },
  });
  assert.equal(utf16.kind, 'result', JSON.stringify(utf16));
  assert.deepEqual(utf16.result, { text: `${text}\uD800` });
  assert.deepEqual(utf16.hooked, ['skill-prompt-test']);
});

test('skill.prompt materializes inherited and getter text at the worker IPC boundary', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, (_api, event) => {
        if (event.text === 'inherited') {
          return Object.create({ text: 'from inherited text' });
        }
        if (event.text === 'getter') {
          return new (class { get text() { return 'from class getter'; } })();
        }
        if (event.text === 'getter-with-extra') {
          return new (class {
            constructor() { this.sourceMarker = 'preserved'; }
            get text() { return 'from getter with extra'; }
          })();
        }
        return { text: event.text };
      });
    }
  `);

  for (const [inputText, expected] of [
    ['inherited', { text: 'from inherited text' }],
    ['getter', { text: 'from class getter' }],
    ['getter-with-extra', { text: 'from getter with extra', sourceMarker: 'preserved' }],
  ]) {
    const frame = await request({
      kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: inputText },
    });
    assert.equal(frame.kind, 'result', JSON.stringify(frame));
    assert.deepEqual(frame.result, expected);
  }
});

test('skill.prompt matches Native check, measure, and final text reads', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    function resultWithTextReads(readText) {
      let textReads = 0;
      const result = Object.create({
        get text() { textReads++; return readText(textReads); },
      });
      Object.defineProperty(result, 'textReads', {
        enumerable: true,
        get: () => textReads,
      });
      return result;
    }

    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, async (_api, event, next) => {
        if (event.text === 'next-same-getter') return await next(event);
        if (event.text === 'third-throws') {
          return resultWithTextReads(reads => {
            if (reads === 3) throw new Error('third text getter read');
            return 'rewritten';
          });
        }
        if (event.text === 'third-is-original') {
          return resultWithTextReads(reads => reads <= 2 ? 'intermediate' : event.text);
        }
        if (event.text === 'fourth-is-output') {
          return resultWithTextReads(reads => reads <= 2 ? 'intermediate'
            : reads === 3 ? 'comparison' : 'final output');
        }
        return { text: event.text };
      });
      on('skill.prompt', { skill: 'insights' }, (_api, event) => {
        if (event.text !== 'next-same-getter') return { text: event.text };
        return resultWithTextReads(reads => reads === 7 ? 'final output' : 'intermediate');
      });
    }
  `);

  const thirdThrows = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'third-throws' },
  });
  assert.equal(thirdThrows.kind, 'error', JSON.stringify(thirdThrows));
  assert.match(thirdThrows.message, /third text getter read/);

  const thirdIsOriginal = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'third-is-original' },
  });
  assert.equal(thirdIsOriginal.kind, 'result', JSON.stringify(thirdIsOriginal));
  assert.deepEqual(thirdIsOriginal.result, { textReads: 3, text: 'third-is-original' });

  const fourthIsOutput = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'fourth-is-output' },
  });
  assert.equal(fourthIsOutput.kind, 'result', JSON.stringify(fourthIsOutput));
  assert.deepEqual(fourthIsOutput.result, { textReads: 4, text: 'final output' });

  const nextSameGetter = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'next-same-getter' },
  });
  assert.equal(nextSameGetter.kind, 'result', JSON.stringify(nextSameGetter));
  assert.deepEqual(nextSameGetter.result, { textReads: 7, text: 'final output' });
});

test('skill.prompt next preserves Native accessor arguments through normalization and downstream use', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    let forwardedTextReads = () => 0;
    function resultFor(text) {
      const result = { text };
      Object.defineProperty(result, 'forwardedTextReads', {
        enumerable: true,
        get: () => forwardedTextReads(),
      });
      return result;
    }

    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, async (_api, event, next) => {
        if (event.text === 'class-forwarded') {
          let reads = 0;
          forwardedTextReads = () => reads;
          const argument = new (class {
            constructor() { this.skill = event.skill; }
            get text() { reads++; return 'class-' + reads; }
          })();
          return await next(argument);
        }
        if (event.text === 'plain-forwarded') {
          let reads = 0;
          forwardedTextReads = () => reads;
          const argument = { skill: event.skill };
          Object.defineProperty(argument, 'text', {
            enumerable: true,
            get() { reads++; return 'plain-' + reads; },
          });
          return await next(argument);
        }
        return { text: event.text };
      });
      on('skill.prompt', { skill: 'insights' }, (_api, event) => {
        const text = event.text;
        if (text === 'class-3' || text === 'plain-1') return resultFor(text);
        return { text };
      });
    }
  `);

  const classGetter = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'class-forwarded' },
  });
  assert.equal(classGetter.kind, 'result', JSON.stringify(classGetter));
  assert.deepEqual(classGetter.result, { text: 'class-3', forwardedTextReads: 4 });

  const plainOwnGetter = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'plain-forwarded' },
  });
  assert.equal(plainOwnGetter.kind, 'result', JSON.stringify(plainOwnGetter));
  assert.deepEqual(plainOwnGetter.result, { text: 'plain-1', forwardedTextReads: 1 });
});

test('skill.prompt next and next.to(core) use the Native terminal text-only result', async t => {
  const source = String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, async (_api, event, next) => {
        if (event.text === 'normal-core') {
          let reads = 0;
          const argument = new (class {
            constructor() { this.skill = event.skill; }
            get text() { reads++; return 'normal-' + reads; }
          })();
          return await next(argument);
        }
        if (event.text === 'hop-core') {
          let reads = 0;
          const argument = new (class {
            constructor() { this.skill = event.skill; }
            get text() { reads++; return 'hop-' + reads; }
          })();
          return await next.to(argument, 'core');
        }
        return { text: event.text };
      });
    }
  `;

  const normal = await startWorker(t);
  await normal.load(source, 'user');
  const normalCore = await normal.request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'normal-core' },
  });
  assert.equal(normalCore.kind, 'result', JSON.stringify(normalCore));
  assert.deepEqual(normalCore.result, { text: 'normal-3' });
  assert.deepEqual(Object.keys(normalCore.result), ['text']);
  assert.equal(normal.pendingCoreEventNames.includes('skill.prompt'), false);

  const hop = await startWorker(t);
  await hop.load(source, 'append');
  const hopCore = await hop.request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'hop-core' },
  });
  assert.equal(hopCore.kind, 'result', JSON.stringify(hopCore));
  assert.deepEqual(hopCore.result, { text: 'hop-1' });
  assert.deepEqual(Object.keys(hopCore.result), ['text']);
  assert.equal(hop.pendingCoreEventNames.includes('skill.prompt'), false);
});

test('skill.prompt next.to(core) preserves Native null-read and result-shape error order', async t => {
  const { load, request, pendingCoreEventNames } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, async (_api, event, next) => {
        if (event.text === 'core-null') return await next.to(null, 'core');
        if (event.text === 'core-number') return await next.to(42, 'core');
        return { text: event.text };
      });
    }
  `, 'prepend');

  const nullEvent = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'core-null' },
  });
  assert.equal(nullEvent.kind, 'error', JSON.stringify(nullEvent));
  assert.match(nullEvent.message, /TypeError:.*event\.text/);

  const numberEvent = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: 'core-number' },
  });
  assert.equal(numberEvent.kind, 'error', JSON.stringify(numberEvent));
  assert.match(numberEvent.message, /core: returned no \{ text \} \(a string\)/);
  assert.deepEqual(pendingCoreEventNames, []);
});

test('skill.prompt pins next arguments, follows next.to(core), and rejects malformed result text', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, async (_api, event, next) => {
        if (event.text === 'next') {
          try { await next({ ...event, skill: 'other' }); }
          catch { return { text: 'next-pinned' }; }
        }
        if (event.text === 'next.to') {
          try { await next.to({ ...event, skill: 'other' }, 'core'); }
          catch { return { text: 'next.to-pinned' }; }
        }
        if (event.text === 'missing') return {};
        if (event.text === 'non-string') return { text: 42 };
        return { text: event.text };
      });
    }
  `, 'prepend');

  for (const [text, expected] of [['next', 'next-pinned'], ['next.to', 'next.to']]) {
    const pinned = await request({
      kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text },
    });
    assert.equal(pinned.kind, 'result', JSON.stringify(pinned));
    assert.deepEqual(pinned.result, { text: expected });
  }

  for (const text of ['missing', 'non-string']) {
    const malformed = await request({
      kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text },
    });
    assert.equal(malformed.kind, 'result', JSON.stringify(malformed));
    assert.deepEqual(malformed.result, { text },
      'a result without string text is skipped and cannot replace the original prompt');
  }
});
